//! Round-trip identity harness — the test spine for the reverse-parser epic (`acb1c6d`).
//!
//! The governing invariant of the round-trip work is `render(parse(render(m))) == render(m)`: a schema
//! object squealy can emit, once rendered to dialect SQL and read back into the neutral model, must
//! re-render to the same SQL — i.e. re-plan to *empty*. This file stands that spine up over a curated
//! corpus of models exercising the view-body and expression idioms a reverse parser must invert.
//!
//! # What this asserts today (Phase 0)
//!
//! The `parse` leg is not yet built (lowering lands in later phases), so this harness currently pins the
//! two ends the invariant is anchored on:
//!
//! 1. [`renders_the_corpus_to_parseable_sql`] — squealy's own rendered output, for every backend that
//!    supports each construct, is accepted by the pinned `sqlparser` for the matching dialect. This is
//!    the precondition for lowering: you cannot invert SQL the parser rejects. Backends that reject a
//!    construct outright (e.g. SQLite has no generated columns) are skipped for that case, not failed.
//! 2. [`reader_entry_points_parse_but_do_not_yet_lower`] — the [`Reader`] seam is wired end-to-end: it
//!    parses squealy's output and reaches the lowering step, which reports [`ReadError::NotYetLowered`].
//!    This documents the current gap the epic closes phase by phase; the test tightens as lowering lands.

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

    // A table CHECK constraint (rendered verbatim; portable spelling across all three backends).
    let mut widgets = plain_table(
        "widgets",
        vec![column("id", SqlType::I32), column("sku", SqlType::Text)],
    );
    widgets.checks = vec![CheckModel {
        name: "sku_len".to_owned(),
        expression: "length(sku) > 0".to_owned(),
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

// ---- tests ----------------------------------------------------------------------------------------

/// Leg one of the round-trip spine: everything squealy renders is parseable by the pinned parser.
///
/// A backend that *rejects* a construct (returns an `io::Error` from `render_create`) is a known
/// backend limitation, not a parse gap — it is counted and skipped. A rendered statement the parser
/// *cannot* parse is a real gap and fails the test with the offending SQL, so the corpus and the parser
/// front-end stay in lockstep as later phases build the lowering on top.
#[test]
fn renders_the_corpus_to_parseable_sql() {
    let corpus = corpus();
    let mut checked = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut gaps: Vec<String> = Vec::new();

    for (case, model) in &corpus {
        for (backend, dialect, render) in BACKENDS {
            match render(model) {
                Err(err) => skipped.push(format!("{case} on {backend}: {err}")),
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
        "sqlparser could not parse squealy's own rendered output for {} case(s):\n\n{}",
        gaps.len(),
        gaps.join("\n\n")
    );
    // The corpus must actually exercise the parser, or a rename/regression could silently gut it.
    assert!(
        checked >= corpus.len(),
        "corpus produced too few parse checks"
    );
}

/// Leg two: the `Reader` seam is wired — it parses squealy's rendered output and reaches the lowering
/// step. Lowering is unimplemented in Phase 0, so every entry point reports `NotYetLowered` (rather than
/// a parse error, which would mean the seam is broken). This test tightens as lowering lands.
#[test]
fn reader_entry_points_parse_but_do_not_yet_lower() {
    let reader = Reader::new(SqlDialect::Postgres);

    // A scalar expression (as a check / generated / index expression would arrive): parses, no lowering.
    match reader.read_check_expression("length(sku) > 0") {
        Err(ReadError::NotYetLowered(_)) => {}
        other => panic!("expected NotYetLowered from read_check_expression, got: {other:?}"),
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
}
