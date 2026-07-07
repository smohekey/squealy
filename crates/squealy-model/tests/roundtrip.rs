//! Round-trip identity harness — the test spine for the reverse-parser epic (`acb1c6d`).
//!
//! The governing invariant of the round-trip work is `render(parse(render(m))) == render(m)`: a schema
//! object squealy can emit, once rendered to dialect SQL and read back into the neutral model, must
//! re-render to the same SQL — i.e. re-plan to *empty*. This file stands that spine up over a curated
//! corpus of models exercising the view-body and expression idioms a reverse parser must invert.
//!
//! # What this asserts
//!
//! 1. [`renders_the_corpus_to_parseable_sql`] — squealy's own rendered output, for every backend that
//!    supports each construct, is accepted by the pinned `sqlparser` for the matching dialect. This is
//!    the precondition for lowering: you cannot invert SQL the parser rejects. Coverage is pinned
//!    exactly: only the documented backend limitations in `EXPECTED_UNSUPPORTED` (e.g. SQLite has no
//!    generated columns) are skipped — any other render failure fails the test as a regression.
//! 2. [`constraint_expressions_round_trip_through_each_dialect`] — the **full spine** for scalar
//!    constraint expressions (Phase 1): a neutral node rendered to each dialect and read back through
//!    the [`Reader`] must lower to the same node and re-render byte-identically. A failure here is the
//!    churn Phase 1 removes.
//! 3. [`reader_entry_points_lower_scalars_but_not_yet_view_bodies`] — the [`Reader`] seam behaves per
//!    entry point: scalar expressions lower structurally; view-body lowering is a later phase and still
//!    reports [`ReadError::NotYetLowered`]. This test tightens as lowering lands.

use std::io;

use squealy::*;
use squealy_parse::{ReadError, Reader, SqlDialect, parse_sql};

const ALIAS: &str = "q0_0";

// ---- expression / model constructors (the model structs mostly don't derive Default) --------------

fn b(expr: ExprNode) -> Box<ExprNode> {
    Box::new(expr)
}

fn col(column: &str) -> ExprNode {
    ExprNode::Column {
        alias: ALIAS.to_owned(),
        column: column.to_owned(),
    }
}

/// An unqualified column, as a constraint / generated / index expression names one.
fn bare(column: &str) -> ExprNode {
    ExprNode::BareColumn {
        column: column.to_owned(),
    }
}

fn lit(text: &str) -> ExprNode {
    ExprNode::Literal(text.to_owned())
}

fn column(name: &str, ty: SqlType) -> ColumnModel {
    ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty,
        collation: None,
        nullable: true,
        default: None,
        identity: None,
        generated: None,
    }
}

fn plain_table(name: &str, columns: Vec<ColumnModel>) -> TableModel {
    TableModel {
        name: name.to_owned(),
        comment: None,
        columns,
        primary_key: None,
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
    }
}

/// The base table every corpus view reads from (aliased `q0_0`).
fn events_table() -> TableModel {
    plain_table(
        "events",
        vec![
            column("id", SqlType::I32),
            column("cnt", SqlType::I64),
            column("amount", SqlType::I64),
            column("name", SqlType::Text),
            column("active", SqlType::Bool),
            column(
                "created",
                SqlType::Timestamp {
                    tz: false,
                    precision: Some(6),
                },
            ),
        ],
    )
}

fn events_source() -> SourceRef {
    SourceRef {
        schema: Some("public".to_owned()),
        name: "events".to_owned(),
        alias: ALIAS.to_owned(),
    }
}

fn proj(output_name: &str, expr: ExprNode) -> ProjectionItem {
    ProjectionItem {
        output_name: output_name.to_owned(),
        expr,
    }
}

/// A view over `events`, with output columns matching `projection` positionally.
fn view(name: &str, outputs: Vec<(&str, SqlType)>, query: ViewQueryModel) -> ViewModel {
    ViewModel {
        name: name.to_owned(),
        comment: None,
        columns: outputs
            .into_iter()
            .map(|(n, ty)| ViewColumnModel {
                name: n.to_owned(),
                ty,
                nullable: true,
            })
            .collect(),
        query,
    }
}

fn body(projection: Vec<ProjectionItem>) -> ViewQueryModel {
    ViewQueryModel {
        projection,
        from: Some(events_source()),
        ..ViewQueryModel::default()
    }
}

/// Wraps a single view (plus the base table it reads) into a one-schema database model.
fn schema_with_view(v: ViewModel) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: vec![events_table()],
            views: vec![v],
        }],
    }
}

/// Wraps a single table into a one-schema database model.
fn schema_with_table(t: TableModel) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: vec![t],
            views: Vec::new(),
        }],
    }
}

// ---- the corpus -----------------------------------------------------------------------------------

/// Every named model, exercising the render idioms the reverse parser must invert. Each renders across
/// all backends that support its constructs; unsupported ones are skipped (not failed) per backend.
// Built by push so each case can be introduced by its own comment; a single `vec!` literal of ten
// deeply-nested models would be far harder to read.
#[allow(clippy::vec_init_then_push)]
fn corpus() -> Vec<(&'static str, DatabaseModel)> {
    let mut cases: Vec<(&'static str, DatabaseModel)> = Vec::new();

    // Arithmetic: add / subtract / multiply / the float-cast division idiom.
    cases.push((
        "view/arithmetic",
        schema_with_view(view(
            "v_arith",
            vec![
                ("added", SqlType::I64),
                ("subbed", SqlType::I64),
                ("mulled", SqlType::I64),
                ("ratio", SqlType::F64),
            ],
            body(vec![
                proj(
                    "added",
                    ExprNode::Binary {
                        op: ArithmeticOp::Add,
                        left: b(col("cnt")),
                        right: b(lit("1")),
                    },
                ),
                proj(
                    "subbed",
                    ExprNode::Binary {
                        op: ArithmeticOp::Subtract,
                        left: b(col("cnt")),
                        right: b(lit("1")),
                    },
                ),
                proj(
                    "mulled",
                    ExprNode::Binary {
                        op: ArithmeticOp::Multiply,
                        left: b(col("cnt")),
                        right: b(lit("2")),
                    },
                ),
                proj(
                    "ratio",
                    ExprNode::Binary {
                        op: ArithmeticOp::Divide,
                        left: b(col("cnt")),
                        right: b(lit("2")),
                    },
                ),
            ]),
        )),
    ));

    // Aggregates with a GROUP BY, and the CAST-result-pin idiom on SUM.
    cases.push((
        "view/aggregate",
        schema_with_view(view(
            "v_agg",
            vec![("total", SqlType::I64), ("n", SqlType::I64)],
            ViewQueryModel {
                projection: vec![
                    proj(
                        "total",
                        ExprNode::Aggregate {
                            func: AggregateFunc::Sum,
                            distinct: false,
                            operand: b(col("amount")),
                            result: Some(SqlType::I64),
                        },
                    ),
                    proj(
                        "n",
                        ExprNode::Aggregate {
                            func: AggregateFunc::Count,
                            distinct: false,
                            operand: b(col("id")),
                            result: None,
                        },
                    ),
                ],
                from: Some(events_source()),
                group_by: vec![col("name")],
                ..ViewQueryModel::default()
            },
        )),
    ));

    // Predicates: comparison, logical AND/OR, NOT, IS NULL.
    cases.push((
        "view/predicate",
        schema_with_view(view(
            "v_pred",
            vec![("id", SqlType::I32)],
            ViewQueryModel {
                projection: vec![proj("id", col("id"))],
                from: Some(events_source()),
                filter: Some(ExprNode::Logical {
                    op: LogicalOp::And,
                    left: b(ExprNode::Compare {
                        op: CompareOp::GreaterThan,
                        left: b(col("cnt")),
                        right: b(lit("10")),
                    }),
                    right: b(ExprNode::Not(b(ExprNode::IsNull {
                        negated: false,
                        operand: b(col("name")),
                    }))),
                }),
                ..ViewQueryModel::default()
            },
        )),
    ));

    // Predicates: LIKE, BETWEEN, IN.
    cases.push((
        "view/predicate-membership",
        schema_with_view(view(
            "v_pred2",
            vec![("id", SqlType::I32)],
            ViewQueryModel {
                projection: vec![proj("id", col("id"))],
                from: Some(events_source()),
                filter: Some(ExprNode::Logical {
                    op: LogicalOp::Or,
                    left: b(ExprNode::Like {
                        case_insensitive: false,
                        negated: false,
                        operand: b(col("name")),
                        pattern: b(lit("'a%'")),
                    }),
                    right: b(ExprNode::Logical {
                        op: LogicalOp::Or,
                        left: b(ExprNode::Between {
                            negated: false,
                            operand: b(col("cnt")),
                            low: b(lit("1")),
                            high: b(lit("10")),
                        }),
                        right: b(ExprNode::In {
                            negated: false,
                            operand: b(col("cnt")),
                            items: vec![lit("1"), lit("2"), lit("3")],
                        }),
                    }),
                }),
                ..ViewQueryModel::default()
            },
        )),
    ));

    // CASE / COALESCE / NULLIF.
    cases.push((
        "view/conditional",
        schema_with_view(view(
            "v_case",
            vec![
                ("grade", SqlType::Text),
                ("amt", SqlType::I64),
                ("nn", SqlType::I64),
            ],
            body(vec![
                proj(
                    "grade",
                    ExprNode::Case {
                        arms: vec![CaseArm {
                            when: b(ExprNode::Compare {
                                op: CompareOp::GreaterThan,
                                left: b(col("cnt")),
                                right: b(lit("10")),
                            }),
                            then: b(lit("'hi'")),
                        }],
                        else_: Some(b(lit("'lo'"))),
                        result: None,
                    },
                ),
                proj(
                    "amt",
                    ExprNode::Coalesce {
                        args: vec![col("amount"), lit("0")],
                        result: None,
                    },
                ),
                proj(
                    "nn",
                    ExprNode::Nullif {
                        left: b(col("cnt")),
                        right: b(lit("0")),
                        result: None,
                    },
                ),
            ]),
        )),
    ));

    // Scalar string functions: Concat (|| vs CONCAT), Substring (substr vs SUBSTRING FROM FOR),
    // Upper, Length (length vs CHAR_LENGTH).
    cases.push((
        "view/string-fns",
        schema_with_view(view(
            "v_str",
            vec![
                ("cc", SqlType::Text),
                ("sub", SqlType::Text),
                ("up", SqlType::Text),
                ("ln", SqlType::I64),
            ],
            body(vec![
                proj(
                    "cc",
                    ExprNode::ScalarFn {
                        func: ScalarFunc::Concat,
                        args: vec![col("name"), lit("'x'")],
                    },
                ),
                proj(
                    "sub",
                    ExprNode::ScalarFn {
                        func: ScalarFunc::Substring,
                        args: vec![col("name"), lit("1"), lit("3")],
                    },
                ),
                proj(
                    "up",
                    ExprNode::ScalarFn {
                        func: ScalarFunc::Upper,
                        args: vec![col("name")],
                    },
                ),
                proj(
                    "ln",
                    ExprNode::ScalarFn {
                        func: ScalarFunc::Length,
                        args: vec![col("name")],
                    },
                ),
            ]),
        )),
    ));

    // Temporal: now() (CURRENT_TIMESTAMP[(6)]) and EXTRACT with a CAST-result pin.
    cases.push((
        "view/temporal",
        schema_with_view(view(
            "v_temporal",
            vec![
                (
                    "n",
                    SqlType::Timestamp {
                        tz: false,
                        precision: Some(6),
                    },
                ),
                ("yr", SqlType::I32),
            ],
            body(vec![
                proj("n", ExprNode::Now),
                proj(
                    "yr",
                    ExprNode::Extract {
                        field: DateField::Year,
                        operand: b(col("created")),
                        result: Some(SqlType::I32),
                        timezone: None,
                    },
                ),
            ]),
        )),
    ));

    // A table CHECK constraint, carried structurally so each backend renders it in its own dialect.
    let mut widgets = plain_table(
        "widgets",
        vec![column("id", SqlType::I32), column("sku", SqlType::Text)],
    );
    widgets.checks = vec![CheckModel {
        name: "sku_len".to_owned(),
        expression: ExprNode::Compare {
            op: CompareOp::GreaterThan,
            left: b(ExprNode::ScalarFn {
                func: ScalarFunc::Length,
                args: vec![ExprNode::BareColumn {
                    column: "sku".to_owned(),
                }],
            }),
            right: b(lit("0")),
        },
        validation: None,
        enforcement: None,
    }];
    cases.push(("table/check", schema_with_table(widgets)));

    // A generated/computed column (PostgreSQL + MySQL; SQLite rejects generated columns → skipped).
    let mut calc = plain_table(
        "calc",
        vec![
            column("base", SqlType::I64),
            column("doubled", SqlType::I64),
        ],
    );
    calc.columns[1].nullable = false;
    calc.columns[1].generated = Some(GeneratedColumnModel {
        expression: "base * 2".to_owned(),
        storage: GeneratedStorage::Stored,
    });
    cases.push(("table/generated", schema_with_table(calc)));

    // A partial index (PostgreSQL + SQLite; MySQL rejects partial predicates → skipped).
    let mut logs = plain_table(
        "logs",
        vec![column("id", SqlType::I32), column("level", SqlType::Text)],
    );
    logs.indexes = vec![IndexModel {
        name: "logs_errors".to_owned(),
        columns: vec!["id".to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        predicate: Some("level = 'error'".to_owned()),
    }];
    cases.push(("table/partial-index", schema_with_table(logs)));

    cases
}

// ---- backends -------------------------------------------------------------------------------------

type RenderFn = fn(&DatabaseModel) -> io::Result<Vec<u8>>;

fn render_postgres(model: &DatabaseModel) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    squealy_postgresql::Postgres.render_create(model, &mut buf)?;
    Ok(buf)
}

fn render_mysql(model: &DatabaseModel) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    squealy_mysql::Mysql.render_create(model, &mut buf)?;
    Ok(buf)
}

fn render_sqlite(model: &DatabaseModel) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    squealy_sqlite::Sqlite.render_create(model, &mut buf)?;
    Ok(buf)
}

const BACKENDS: [(&str, SqlDialect, RenderFn); 3] = [
    ("postgres", SqlDialect::Postgres, render_postgres),
    ("mysql", SqlDialect::Mysql, render_mysql),
    ("sqlite", SqlDialect::Sqlite, render_sqlite),
];

/// The `(case, backend)` pairs a backend legitimately cannot render — a documented backend limitation,
/// not a parse gap. Every *other* render failure is a regression the harness must fail on (not silently
/// skip). Keep this exhaustive: an entry that stops applying (the backend gains support) is caught by
/// the exact-coverage assertion, forcing this list to stay honest.
const EXPECTED_UNSUPPORTED: &[(&str, &str)] = &[
    ("table/generated", "sqlite"),    // SQLite has no generated columns.
    ("table/partial-index", "mysql"), // MySQL has no partial index predicates.
];

// ---- tests ----------------------------------------------------------------------------------------

/// Leg one of the round-trip spine: everything squealy renders is parseable by the pinned parser.
///
/// Every `(case, backend)` pair must resolve one of exactly two ways: the model renders and the parser
/// accepts it, or the pair is a documented backend limitation in [`EXPECTED_UNSUPPORTED`]. Any other
/// outcome fails the test — a parse gap (the parser rejects squealy's own output) *or* an unexpected
/// render failure (a supported model regressed, or a whole backend broke). Coverage is pinned exactly,
/// so a silently-dropped pair can't let a regression slip through green.
#[test]
fn renders_the_corpus_to_parseable_sql() {
    let corpus = corpus();
    let mut checked = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut gaps: Vec<String> = Vec::new();

    for (case, model) in &corpus {
        for (backend, dialect, render) in BACKENDS {
            let expected_unsupported = EXPECTED_UNSUPPORTED.contains(&(case, backend));
            match render(model) {
                Err(err) if expected_unsupported => {
                    skipped.push(format!("{case} on {backend}: {err}"))
                }
                // A render failure for a pair we expect to support is a regression, not a skip.
                Err(err) => gaps.push(format!(
                    "{case} on {backend}: unexpected render failure: {err}"
                )),
                // A pair we listed as unsupported that now renders means the whitelist is stale.
                Ok(_) if expected_unsupported => gaps.push(format!(
                    "{case} on {backend}: rendered though listed unsupported — remove it from \
                     EXPECTED_UNSUPPORTED"
                )),
                Ok(bytes) => {
                    let sql = String::from_utf8(bytes).expect("rendered SQL is valid UTF-8");
                    match parse_sql(&sql, dialect) {
                        Ok(statements) => {
                            assert!(
                                !statements.is_empty(),
                                "{case} on {backend}: rendered SQL parsed to zero statements"
                            );
                            checked += 1;
                        }
                        Err(err) => {
                            gaps.push(format!("{case} on {backend}: {err}\n--- SQL ---\n{sql}"))
                        }
                    }
                }
            }
        }
    }

    // Baseline visibility: what the corpus actually covered vs. skipped (printed with `--nocapture`).
    eprintln!(
        "round-trip corpus: {checked} rendered+parsed, {} skipped (backend-unsupported)",
        skipped.len()
    );
    for note in &skipped {
        eprintln!("  skipped: {note}");
    }

    assert!(
        gaps.is_empty(),
        "round-trip corpus failed for {} pair(s):\n\n{}",
        gaps.len(),
        gaps.join("\n\n")
    );
    // Exact coverage: every corpus × backend pair is either parsed or a known-unsupported skip, nothing
    // silently dropped. If a backend gains support for a whitelisted pair, `checked` rises and this
    // fails — forcing EXPECTED_UNSUPPORTED to be pruned.
    assert_eq!(
        checked,
        corpus.len() * BACKENDS.len() - EXPECTED_UNSUPPORTED.len(),
        "unexpected number of parse checks — coverage drifted from the whitelist"
    );
}

/// Leg two: the `Reader` seam behaves per entry point. Scalar expressions (check / generated / index)
/// now *lower* into an [`ExprNode`]; view-body lowering is a later phase and still reports
/// `NotYetLowered` (rather than a parse error, which would mean the seam is broken). This test tightens
/// as lowering lands.
#[test]
fn reader_entry_points_lower_scalars_but_not_yet_view_bodies() {
    let reader = Reader::new(SqlDialect::Postgres);

    // A scalar expression (as a check / generated / index expression arrives) lowers structurally.
    match reader.read_check_expression("(CHAR_LENGTH(\"sku\") > 0)") {
        Ok(ExprNode::Compare {
            op: CompareOp::GreaterThan,
            ..
        }) => {}
        other => panic!("expected a lowered comparison from read_check_expression, got: {other:?}"),
    }

    // A scalar shape outside the covered grammar (`%` has no neutral node) is reported, not mislowered.
    match reader.read_check_expression("(\"a\" % 2)") {
        Err(ReadError::NotYetLowered(_)) => {}
        other => panic!("expected NotYetLowered for modulo, got: {other:?}"),
    }

    // A CREATE VIEW: parses to the right statement shape, then reports lowering unimplemented.
    match reader.read_create_view("CREATE VIEW v AS SELECT 1 AS n") {
        Err(ReadError::NotYetLowered(_)) => {}
        other => panic!("expected NotYetLowered from read_create_view, got: {other:?}"),
    }

    // A non-view statement handed to the view entry point is a shape error, not NotYetLowered.
    match reader.read_create_view("SELECT 1") {
        Err(ReadError::Unexpected(_)) => {}
        other => panic!("expected Unexpected for a non-CREATE-VIEW statement, got: {other:?}"),
    }

    // Genuinely unparseable text surfaces as a parse error.
    match reader.read_check_expression("length(sku >") {
        Err(ReadError::Parse { .. }) => {}
        other => panic!("expected Parse error for malformed SQL, got: {other:?}"),
    }

    // A valid expression prefix with trailing junk must not be silently truncated to the prefix.
    match reader.read_check_expression("length(sku) > 0 junk") {
        Err(ReadError::Unexpected(_)) => {}
        other => panic!("expected Unexpected for trailing tokens, got: {other:?}"),
    }

    // A multi-expression index key (as `pg_get_expr` returns it, comma-joined) splits into one structural
    // term per expression — a comma inside a call (`substring(x, 1, 2)`) stays part of its term.
    assert_eq!(
        reader.read_index_expressions_or_raw("lower(slug), substring(x, 1, 2)"),
        vec![
            ExprNode::ScalarFn {
                func: ScalarFunc::Lower,
                args: vec![bare("slug")],
            },
            ExprNode::ScalarFn {
                func: ScalarFunc::Substring,
                args: vec![
                    bare("x"),
                    ExprNode::Literal("1".to_owned()),
                    ExprNode::Literal("2".to_owned()),
                ],
            },
        ],
    );

    // If any term cannot be lowered (a `::text` cast on a non-literal), the whole key is preserved as one
    // verbatim `Raw` rather than a partial/garbled split.
    assert_eq!(
        reader.read_index_expressions_or_raw("lower((slug)::text), upper(name)"),
        vec![ExprNode::Raw("lower((slug)::text), upper(name)".to_owned())],
    );
}

// ---- constraint / generated / index expression round-trip -----------------------------------------

/// Neutral scalar expressions, as a `CHECK` / generated-column / index term would carry (unqualified
/// columns via [`bare`]). Each must survive `render → parse → lower → render` on every backend: rendered
/// to that dialect's SQL, read back through [`Reader`], the lowered [`ExprNode`] must equal the original
/// *and* re-render byte-identically. This exercises the render idioms lowering has to invert — full
/// parenthesization, the float-cast division form, `||`/`CONCAT`, `SUBSTRING …/substr`, `CHAR_LENGTH`/
/// `length` — across all three dialects from a single neutral node.
#[allow(clippy::vec_init_then_push)]
fn constraint_corpus() -> Vec<(&'static str, ExprNode)> {
    let mut cases: Vec<(&'static str, ExprNode)> = Vec::new();

    let cmp = |op, left, right| ExprNode::Compare {
        op,
        left: b(left),
        right: b(right),
    };

    // Comparison against a literal.
    cases.push((
        "compare",
        cmp(CompareOp::GreaterThan, bare("cnt"), lit("0")),
    ));

    // A conjunction of range bounds (nested, fully parenthesized).
    cases.push((
        "logical-range",
        ExprNode::Logical {
            op: LogicalOp::And,
            left: b(cmp(CompareOp::GreaterThanOrEquals, bare("price"), lit("0"))),
            right: b(cmp(CompareOp::LessThanOrEquals, bare("price"), lit("1000"))),
        },
    ));

    // `CHAR_LENGTH` (PostgreSQL/MySQL) vs `length` (SQLite) folding.
    cases.push((
        "length",
        cmp(
            CompareOp::GreaterThan,
            ExprNode::ScalarFn {
                func: ScalarFunc::Length,
                args: vec![bare("sku")],
            },
            lit("0"),
        ),
    ));

    // The float-cast division idiom (PostgreSQL `double precision` / SQLite `REAL` casts; MySQL none).
    cases.push((
        "division",
        cmp(
            CompareOp::GreaterThan,
            ExprNode::Binary {
                op: ArithmeticOp::Divide,
                left: b(bare("total")),
                right: b(bare("qty")),
            },
            lit("0"),
        ),
    ));

    // `IN (<list>)`.
    cases.push((
        "in-list",
        ExprNode::In {
            negated: false,
            operand: b(bare("status")),
            items: vec![lit("1"), lit("2"), lit("3")],
        },
    ));

    // `BETWEEN`.
    cases.push((
        "between",
        ExprNode::Between {
            negated: false,
            operand: b(bare("qty")),
            low: b(lit("1")),
            high: b(lit("10")),
        },
    ));

    // `LIKE`.
    cases.push((
        "like",
        ExprNode::Like {
            case_insensitive: false,
            negated: false,
            operand: b(bare("code")),
            pattern: b(lit("'A%'")),
        },
    ));

    // Concatenation: `||` (PostgreSQL/SQLite) vs `CONCAT(...)` (MySQL).
    cases.push((
        "concat",
        cmp(
            CompareOp::NotEquals,
            ExprNode::ScalarFn {
                func: ScalarFunc::Concat,
                args: vec![bare("first"), bare("last")],
            },
            lit("''"),
        ),
    ));

    // Chained concatenation is binary-nested in the model (`a.concat(b).concat(c)`), so it renders as
    // `((a || b) || c)` (PG/SQLite) / `CONCAT(CONCAT(a, b), c)` (MySQL) — NOT a flat chain — and must
    // round-trip without flattening. Both left- and right-nested shapes are exercised.
    let concat = |left, right| ExprNode::ScalarFn {
        func: ScalarFunc::Concat,
        args: vec![left, right],
    };
    cases.push((
        "concat-nested-left",
        concat(concat(bare("a"), bare("b")), bare("c")),
    ));
    cases.push((
        "concat-nested-right",
        concat(bare("a"), concat(bare("b"), bare("c"))),
    ));

    // The empty-`IN` / empty-`NOT IN` sentinels (`<op> IS NOT NULL AND 1 = 0` / `… OR 1 = 1`).
    cases.push((
        "empty-in",
        ExprNode::In {
            negated: false,
            operand: b(bare("status")),
            items: Vec::new(),
        },
    ));
    cases.push((
        "empty-not-in",
        ExprNode::In {
            negated: true,
            operand: b(bare("status")),
            items: Vec::new(),
        },
    ));

    // `SUBSTRING(s FROM a FOR b)` (PostgreSQL/MySQL) vs `substr(s, a, b)` (SQLite).
    cases.push((
        "substring",
        cmp(
            CompareOp::Equals,
            ExprNode::ScalarFn {
                func: ScalarFunc::Substring,
                args: vec![bare("code"), lit("1"), lit("1")],
            },
            lit("'A'"),
        ),
    ));

    // `IS NULL` and `NOT`.
    cases.push((
        "is-null",
        ExprNode::IsNull {
            negated: false,
            operand: b(bare("deleted_at")),
        },
    ));
    cases.push(("not", ExprNode::Not(b(bare("active")))));

    cases
}

fn render_scalar(node: &ExprNode, dialect: &dyn squealy::Dialect) -> String {
    let mut buf = Vec::new();
    squealy::render_scalar_expr(node, dialect, &mut buf).expect("scalar expression renders");
    String::from_utf8(buf).expect("rendered SQL is valid UTF-8")
}

/// The full round-trip spine for scalar constraint expressions: a neutral node, rendered to each
/// dialect and read back, must lower to the same node and re-render identically. A gap here is exactly
/// the churn Phase 1 removes (an introspected constraint that will not re-plan to empty).
#[test]
fn constraint_expressions_round_trip_through_each_dialect() {
    let dialects: [(&str, SqlDialect, &dyn squealy::Dialect); 3] = [
        (
            "postgres",
            SqlDialect::Postgres,
            &squealy_postgresql::Postgres.dialect(),
        ),
        ("mysql", SqlDialect::Mysql, &squealy_mysql::Mysql.dialect()),
        (
            "sqlite",
            SqlDialect::Sqlite,
            &squealy_sqlite::Sqlite.dialect(),
        ),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (case, node) in constraint_corpus() {
        for (backend, sql_dialect, dialect) in dialects {
            let rendered = render_scalar(&node, dialect);
            let lowered = match Reader::new(sql_dialect).read_check_expression(&rendered) {
                Ok(lowered) => lowered,
                Err(err) => {
                    failures.push(format!(
                        "{case} on {backend}: read failed: {err}\n  SQL: {rendered}"
                    ));
                    continue;
                }
            };
            if lowered != node {
                failures.push(format!(
                    "{case} on {backend}: lowered node differs from the original\n  SQL: {rendered}\n  got: {lowered:?}"
                ));
                continue;
            }
            let re_rendered = render_scalar(&lowered, dialect);
            if re_rendered != rendered {
                failures.push(format!(
                    "{case} on {backend}: re-rendered SQL differs\n  first:  {rendered}\n  second: {re_rendered}"
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "constraint round-trip failed for {} case(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
