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
//! 3. [`reader_entry_points_lower_scalars_and_single_select_view_bodies`] — the [`Reader`] seam behaves
//!    per entry point: scalar expressions lower structurally, and a single-`SELECT` view body now lowers
//!    into a [`ViewQueryModel`]. This test tightens as lowering lands.
//! 4. [`view_bodies_round_trip_through_each_dialect`] — the **view-body spine** (PR 2.0): a neutral
//!    [`ViewModel`] rendered to each dialect and read back must re-render byte-identically (the epic
//!    invariant), and — where the dialect's cast vocabulary is lossless — lower to the same model.

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

fn events_source() -> SourceItem {
    SourceItem::Named(SourceRef {
        schema: Some("public".to_owned()),
        name: "events".to_owned(),
        alias: ALIAS.to_owned(),
    })
}

fn proj(output_name: &str, expr: ExprNode) -> ProjectionItem {
    ProjectionItem {
        output_name: output_name.to_owned(),
        internal_alias: None,
        expr,
    }
}

/// A projection whose own `AS <internal_alias>` differs from the view-output column name a column list
/// declares — the body's own clauses reference `internal_alias`, so the renderer must re-emit it.
fn proj_aliased(output_name: &str, internal_alias: &str, expr: ExprNode) -> ProjectionItem {
    ProjectionItem {
        output_name: output_name.to_owned(),
        internal_alias: Some(internal_alias.to_owned()),
        expr,
    }
}

/// Wraps a single `SELECT` [`ViewQueryModel`] as a [`ViewBody`].
fn sel(query: ViewQueryModel) -> ViewBody {
    ViewBody::Select(Box::new(query))
}

/// A view over `events` whose body is a single `SELECT`, with output columns matching `projection`
/// positionally.
fn view(name: &str, outputs: Vec<(&str, SqlType)>, query: ViewQueryModel) -> ViewModel {
    view_of(name, outputs, sel(query))
}

/// A view over `events` whose body is any [`ViewBody`] (used for set-operation cases).
fn view_of(name: &str, outputs: Vec<(&str, SqlType)>, query: ViewBody) -> ViewModel {
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

    // Arithmetic: add / subtract / multiply / modulo / the float-cast division idiom.
    cases.push((
        "view/arithmetic",
        schema_with_view(view(
            "v_arith",
            vec![
                ("added", SqlType::I64),
                ("subbed", SqlType::I64),
                ("mulled", SqlType::I64),
                ("modded", SqlType::I64),
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
                    "modded",
                    ExprNode::Binary {
                        op: ArithmeticOp::Modulo,
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

    // A derived-table (subquery) FROM source: the outer view selects from `(SELECT …) AS q0_0`. This
    // exercises the `SourceItem::Derived` IR widening end to end (render → parse → lower → render), incl.
    // the inner body's own `FROM`/`WHERE` and its aliased projections.
    let inner_col = |column: &str| ExprNode::Column {
        alias: "q1_0".to_owned(),
        column: column.to_owned(),
    };
    cases.push((
        "view/derived-table",
        schema_with_view(view(
            "v_derived",
            vec![("id", SqlType::I32), ("cnt", SqlType::I64)],
            ViewQueryModel {
                projection: vec![proj("id", col("id")), proj("cnt", col("cnt"))],
                from: Some(SourceItem::Derived {
                    query: Box::new(sel(ViewQueryModel {
                        projection: vec![
                            proj("id", inner_col("id")),
                            proj("cnt", inner_col("cnt")),
                        ],
                        from: Some(SourceItem::Named(SourceRef {
                            schema: Some("public".to_owned()),
                            name: "events".to_owned(),
                            alias: "q1_0".to_owned(),
                        })),
                        filter: Some(ExprNode::Compare {
                            op: CompareOp::GreaterThan,
                            left: b(inner_col("cnt")),
                            right: b(lit("0")),
                        }),
                        ..ViewQueryModel::default()
                    })),
                    alias: "q0_0".to_owned(),
                }),
                ..ViewQueryModel::default()
            },
        )),
    ));

    // A set-operation (`UNION`) view body — exercises the recursive `ViewBody::Set` IR widening end to
    // end (render → parse → lower → render), including the per-dialect set-operand wrapping (PG/MySQL
    // `(…)`, SQLite `SELECT * FROM (…)`). Both arms select `id` from `events` under complementary filters.
    let arm_col = |alias: &str, column: &str| ExprNode::Column {
        alias: alias.to_owned(),
        column: column.to_owned(),
    };
    let arm = |alias: &str, op: CompareOp| {
        sel(ViewQueryModel {
            projection: vec![proj("id", arm_col(alias, "id"))],
            from: Some(SourceItem::Named(SourceRef {
                schema: Some("public".to_owned()),
                name: "events".to_owned(),
                alias: alias.to_owned(),
            })),
            filter: Some(ExprNode::Compare {
                op,
                left: b(arm_col(alias, "cnt")),
                right: b(lit("0")),
            }),
            ..ViewQueryModel::default()
        })
    };
    cases.push((
        "view/set-op",
        schema_with_view(view_of(
            "v_setop",
            vec![("id", SqlType::I32)],
            ViewBody::Set {
                op: ViewSetOp::Union,
                all: false,
                left: Box::new(arm("q0_0", CompareOp::GreaterThan)),
                right: Box::new(arm("q0_1", CompareOp::LessThanOrEquals)),
                // A whole-set `ORDER BY` + `LIMIT` on the compound. The order term references the *output*
                // column `id` by name (a bare column) — a compound `ORDER BY` binds to outputs, not arms.
                order_by: vec![OrderItem {
                    expr: bare("id"),
                    direction: Some(OrderDirection::Desc),
                    nulls: None,
                }],
                limit: Some(10),
                offset: None,
            },
        )),
    ));

    // Set-op arms that carry their *own* clauses, on a **column-listed** view whose output column name
    // (`n`) deliberately DIFFERS from the arms' projection aliases (`total`). Both arms project an
    // aliased *expression* (so the alias is the only name to lower it back through), and the **leftmost**
    // arm also has a per-arm `ORDER BY <alias>` + `LIMIT`. Each arm's alias must survive independently of
    // the view column list — the list names only the compound output (kept on `ViewModel.columns`), so
    // it must not overwrite the arms' aliases (else `ORDER BY total` dangles on re-render).
    let named_events = |alias: &str| {
        SourceItem::Named(SourceRef {
            schema: Some("public".to_owned()),
            name: "events".to_owned(),
            alias: alias.to_owned(),
        })
    };
    let scaled = |alias: &str, op: ArithmeticOp| {
        proj(
            "total",
            ExprNode::Binary {
                op,
                left: b(ExprNode::Column {
                    alias: alias.to_owned(),
                    column: "amount".to_owned(),
                }),
                right: b(lit("2")),
            },
        )
    };
    cases.push((
        "view/set-op-arm-clause",
        schema_with_view(view_of(
            "v_setop_arm",
            vec![("n", SqlType::I64)],
            ViewBody::Set {
                op: ViewSetOp::Union,
                all: false,
                left: Box::new(sel(ViewQueryModel {
                    projection: vec![scaled("q0_0", ArithmeticOp::Multiply)],
                    from: Some(named_events("q0_0")),
                    // A per-arm ORDER BY on the *leftmost* arm's own projection alias, plus a per-arm LIMIT.
                    order_by: vec![OrderItem {
                        expr: bare("total"),
                        direction: None,
                        nulls: None,
                    }],
                    limit: Some(5),
                    ..ViewQueryModel::default()
                })),
                right: Box::new(sel(ViewQueryModel {
                    projection: vec![scaled("q0_1", ArithmeticOp::Add)],
                    from: Some(named_events("q0_1")),
                    ..ViewQueryModel::default()
                })),
                order_by: Vec::new(),
                limit: None,
                offset: None,
            },
        )),
    ));

    // A **column-listed** single-`SELECT` view whose body's own `ORDER BY` references a *computed*
    // projection's own `AS` alias (`total`), which the declared output column (`n`) renames — so the
    // column list suppresses the `AS`, yet the clause still needs it. The projection keeps its inner alias
    // in [`ProjectionItem::internal_alias`]; the renderer re-emits `AS total` under the `(n)` list so
    // `ORDER BY total` resolves (else the DDL is invalid — git-bug e1d0724). Round-trips to itself.
    cases.push((
        "view/single-select-order-alias",
        schema_with_view(view_of(
            "v_alias_order",
            vec![("n", SqlType::I64)],
            sel(ViewQueryModel {
                projection: vec![proj_aliased(
                    "n",
                    "total",
                    ExprNode::Binary {
                        op: ArithmeticOp::Multiply,
                        left: b(col("amount")),
                        right: b(lit("2")),
                    },
                )],
                from: Some(events_source()),
                order_by: vec![OrderItem {
                    expr: bare("total"),
                    direction: None,
                    nulls: None,
                }],
                ..ViewQueryModel::default()
            }),
        )),
    ));

    // The inner alias may *coincide* with the declared output column (`total` == `total`) and still be
    // required: a column list does not introduce its names into the `SELECT` scope, so `SELECT 1 (total)
    // ORDER BY total` would dangle without the explicit `AS total`. The renderer re-emits it; PostgreSQL and
    // MySQL reject the bare form, so this must round-trip with the alias present (git-bug e1d0724).
    cases.push((
        "view/single-select-alias-coincides-output",
        schema_with_view(view_of(
            "v_alias_coincide",
            vec![("total", SqlType::I64)],
            sel(ViewQueryModel {
                projection: vec![proj_aliased(
                    "total",
                    "total",
                    ExprNode::Binary {
                        op: ArithmeticOp::Multiply,
                        left: b(col("amount")),
                        right: b(lit("2")),
                    },
                )],
                from: Some(events_source()),
                order_by: vec![OrderItem {
                    expr: bare("total"),
                    direction: None,
                    nulls: None,
                }],
                ..ViewQueryModel::default()
            }),
        )),
    ));

    // A *plain-column* projection carrying an explicit `AS` (`q0_0.amount AS inner`) that the column list
    // renames to `n`, with `ORDER BY inner` naming that suppressed alias. The kept `internal_alias` must be
    // re-emitted even though the projection is a column, not an expression (git-bug e1d0724).
    cases.push((
        "view/single-select-plain-column-alias",
        schema_with_view(view_of(
            "v_alias_plain",
            vec![("n", SqlType::I64)],
            sel(ViewQueryModel {
                projection: vec![proj_aliased("n", "inner", col("amount"))],
                from: Some(events_source()),
                order_by: vec![OrderItem {
                    expr: bare("inner"),
                    direction: None,
                    nulls: None,
                }],
                ..ViewQueryModel::default()
            }),
        )),
    ));

    // The `GROUP BY` sibling of the case above: a column-listed view whose `GROUP BY` names a computed
    // projection's inner alias (`total`), renamed to the output column `bucket`. A second, un-aliased
    // aggregate projection (`cnt`) shares the suppressing column list. `GROUP BY total` must re-resolve.
    cases.push((
        "view/single-select-group-alias",
        schema_with_view(view_of(
            "v_alias_group",
            vec![("bucket", SqlType::I64), ("cnt", SqlType::I64)],
            sel(ViewQueryModel {
                projection: vec![
                    proj_aliased(
                        "bucket",
                        "total",
                        ExprNode::Binary {
                            op: ArithmeticOp::Multiply,
                            left: b(col("amount")),
                            right: b(lit("2")),
                        },
                    ),
                    proj(
                        "cnt",
                        ExprNode::Aggregate {
                            func: AggregateFunc::Count,
                            distinct: false,
                            operand: b(col("id")),
                            result: None,
                        },
                    ),
                ],
                from: Some(events_source()),
                group_by: vec![bare("total")],
                ..ViewQueryModel::default()
            }),
        )),
    ));

    // The same suppressed-alias shape inside a **CTE** body (`ViewBody::With` re-threads the column-list
    // suppression to the CTE's own `SELECT`): the CTE `scaled (n)` renames its computed `total` projection,
    // and its own `ORDER BY total` must re-resolve. The outer body selects the renamed column back out.
    cases.push((
        "view/cte-alias-clause",
        schema_with_view(view_of(
            "v_cte_alias",
            vec![("n", SqlType::I64)],
            ViewBody::With {
                recursive: false,
                ctes: vec![CteModel {
                    name: "scaled".to_owned(),
                    columns: vec!["n".to_owned()],
                    body: sel(ViewQueryModel {
                        projection: vec![proj_aliased(
                            "n",
                            "total",
                            ExprNode::Binary {
                                op: ArithmeticOp::Multiply,
                                left: b(col("amount")),
                                right: b(lit("2")),
                            },
                        )],
                        from: Some(events_source()),
                        order_by: vec![OrderItem {
                            expr: bare("total"),
                            direction: None,
                            nulls: None,
                        }],
                        ..ViewQueryModel::default()
                    }),
                }],
                body: Box::new(sel(ViewQueryModel {
                    projection: vec![proj(
                        "n",
                        ExprNode::Column {
                            alias: "q1_0".to_owned(),
                            column: "n".to_owned(),
                        },
                    )],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: None,
                        name: "scaled".to_owned(),
                        alias: "q1_0".to_owned(),
                    })),
                    ..ViewQueryModel::default()
                })),
            },
        )),
    ));

    // A `WITH` (CTE) view body — exercises the `ViewBody::With` IR widening end to end (render → parse →
    // lower → render). A single, non-recursive, un-column-listed CTE `recent` filters `events`; the main
    // body selects from it by the bound CTE name (`recent`, a schema-less `SourceItem::Named` that the
    // WITH-scoping recognizes as a local binding, not an external dependency).
    let cte_inner_col = |column: &str| ExprNode::Column {
        alias: "q1_0".to_owned(),
        column: column.to_owned(),
    };
    cases.push((
        "view/cte",
        schema_with_view(view_of(
            "v_cte",
            vec![("id", SqlType::I32)],
            ViewBody::With {
                recursive: false,
                ctes: vec![CteModel {
                    name: "recent".to_owned(),
                    // No declared column list — the CTE body's aliased projections name its outputs.
                    columns: Vec::new(),
                    body: sel(ViewQueryModel {
                        projection: vec![
                            proj("id", cte_inner_col("id")),
                            proj("cnt", cte_inner_col("cnt")),
                        ],
                        from: Some(SourceItem::Named(SourceRef {
                            schema: Some("public".to_owned()),
                            name: "events".to_owned(),
                            alias: "q1_0".to_owned(),
                        })),
                        filter: Some(ExprNode::Compare {
                            op: CompareOp::GreaterThan,
                            left: b(cte_inner_col("cnt")),
                            right: b(lit("0")),
                        }),
                        ..ViewQueryModel::default()
                    }),
                }],
                body: Box::new(sel(ViewQueryModel {
                    projection: vec![proj("id", col("id"))],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: None,
                        name: "recent".to_owned(),
                        alias: ALIAS.to_owned(),
                    })),
                    ..ViewQueryModel::default()
                })),
            },
        )),
    ));

    // A **recursive** `WITH RECURSIVE` view body — a classic integer counter. The CTE `counter` carries a
    // declared column list (`n`); its body is a `ViewBody::Set { Union, all }` whose recursive arm's `FROM`
    // references the CTE name (`counter`). The renderer detects that self-reference structurally and emits
    // the arms **bare** — `<anchor> UNION ALL <recursive>` with no operand wrapping — on every dialect (a
    // recursive-CTE grammar rejects a parenthesized arm; SQLite errors on `(SELECT …)` or `SELECT * FROM
    // (…)` alike).
    cases.push((
        "view/recursive-cte",
        schema_with_view(view_of(
            "v_recursive_cte",
            vec![("n", SqlType::I32)],
            ViewBody::With {
                recursive: true,
                ctes: vec![CteModel {
                    name: "counter".to_owned(),
                    columns: vec!["n".to_owned()],
                    body: ViewBody::Set {
                        op: ViewSetOp::Union,
                        all: true,
                        // Anchor: `SELECT 1` (no FROM).
                        left: Box::new(sel(ViewQueryModel {
                            projection: vec![proj("n", lit("1"))],
                            ..ViewQueryModel::default()
                        })),
                        // Recursive arm: `SELECT counter.n + 1 FROM counter WHERE counter.n < 10`.
                        right: Box::new(sel(ViewQueryModel {
                            projection: vec![proj(
                                "n",
                                ExprNode::Binary {
                                    op: ArithmeticOp::Add,
                                    left: b(col("n")),
                                    right: b(lit("1")),
                                },
                            )],
                            from: Some(SourceItem::Named(SourceRef {
                                schema: None,
                                name: "counter".to_owned(),
                                alias: ALIAS.to_owned(),
                            })),
                            filter: Some(ExprNode::Compare {
                                op: CompareOp::LessThan,
                                left: b(col("n")),
                                right: b(lit("10")),
                            }),
                            ..ViewQueryModel::default()
                        })),
                        order_by: Vec::new(),
                        limit: None,
                        offset: None,
                    },
                }],
                body: Box::new(sel(ViewQueryModel {
                    projection: vec![proj("n", col("n"))],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: None,
                        name: "counter".to_owned(),
                        alias: ALIAS.to_owned(),
                    })),
                    ..ViewQueryModel::default()
                })),
            },
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
        expression: Some(ExprNode::Binary {
            op: ArithmeticOp::Multiply,
            left: Box::new(bare("base")),
            right: Box::new(ExprNode::Literal("2".to_owned())),
        }),
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
        predicate: Some(Box::new(ExprNode::Compare {
            op: CompareOp::Equals,
            left: Box::new(bare("level")),
            right: Box::new(ExprNode::Literal("'error'".to_owned())),
        })),
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
/// lower into an [`ExprNode`]; a single-`SELECT` view body lowers into a [`ViewQueryModel`]. A shape
/// error (a non-view statement) or a parse error must stay distinct from `NotYetLowered`. This test
/// tightens as lowering lands.
#[test]
fn reader_entry_points_lower_scalars_and_single_select_view_bodies() {
    let reader = Reader::new(SqlDialect::Postgres);

    // A scalar expression (as a check / generated / index expression arrives) lowers structurally.
    match reader.read_check_expression("(CHAR_LENGTH(\"sku\") > 0)") {
        Ok(ExprNode::Compare {
            op: CompareOp::GreaterThan,
            ..
        }) => {}
        other => panic!("expected a lowered comparison from read_check_expression, got: {other:?}"),
    }

    // A partial-index predicate (a boolean `WHERE`) lowers structurally, as unqualified bare columns.
    assert_eq!(
        reader.read_index_predicate_or_raw("(deleted_at IS NULL)"),
        ExprNode::IsNull {
            negated: false,
            operand: Box::new(bare("deleted_at")),
        },
    );

    // `%` modulo lowers structurally to a neutral `Modulo` binary (the same operator on every dialect).
    assert_eq!(
        reader
            .read_check_expression("(\"a\" % 2)")
            .expect("modulo lowers to a neutral node"),
        ExprNode::Binary {
            op: ArithmeticOp::Modulo,
            left: Box::new(bare("a")),
            right: Box::new(lit("2")),
        },
    );

    // A CREATE VIEW body now lowers: the aliased projection `1 AS n` names the single output `n`, and a
    // constant SELECT has no `FROM`.
    assert_eq!(
        reader
            .read_create_view("CREATE VIEW v AS SELECT 1 AS n")
            .expect("a single-SELECT view body lowers"),
        sel(ViewQueryModel {
            projection: vec![proj("n", lit("1"))],
            ..ViewQueryModel::default()
        }),
    );

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

    // If any term cannot be lowered (here a cast to a non-modeled type — `::inet` has no `SqlType`), the
    // whole key is preserved as one verbatim `Raw` rather than a partial/garbled split. (A `::text` cast
    // now DOES lower to a structural `Cast`, so it no longer keeps the key Raw.)
    assert_eq!(
        reader.read_index_expressions_or_raw("lower((slug)::inet), upper(name)"),
        vec![ExprNode::Raw("lower((slug)::inet), upper(name)".to_owned())],
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

    // `%` modulo — the same operator on every dialect, no float-cast idiom (unlike division). This is
    // the residual check shape that kept `canonical.rs` alive; it now round-trips structurally.
    cases.push((
        "modulo",
        cmp(
            CompareOp::Equals,
            ExprNode::Binary {
                op: ArithmeticOp::Modulo,
                left: b(bare("amount")),
                right: b(lit("2")),
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

    // A general (non-closed-set) function call — the case that lets PostgreSQL's `canonical.rs` string
    // normalizer be deleted. Its name renders verbatim on every dialect and lowers back to the same
    // `Function` node.
    cases.push((
        "general-function",
        cmp(
            CompareOp::Equals,
            ExprNode::Function {
                name: "md5".to_string(),
                args: vec![bare("sku")],
            },
            lit("'x'"),
        ),
    ));

    // A general function with multiple (comma-separated) column arguments. Only column/expression
    // arguments structure — a direct literal argument stays `Raw` (pg would synthesize an arg cast that
    // cannot be stripped without risking a different overload), so it is not exercised here.
    cases.push((
        "general-function-multi-arg",
        cmp(
            CompareOp::GreaterThan,
            ExprNode::Function {
                name: "custom_fn".to_string(),
                args: vec![bare("path"), bare("host")],
            },
            lit("0"),
        ),
    ));

    // A general function nested as an argument of another general function (`abs` is outside the closed
    // set, so it stays a `Function` node rather than folding).
    cases.push((
        "general-function-nested",
        cmp(
            CompareOp::Equals,
            ExprNode::Function {
                name: "md5".to_string(),
                args: vec![ExprNode::Function {
                    name: "abs".to_string(),
                    args: vec![bare("sku")],
                }],
            },
            lit("'x'"),
        ),
    ));

    // A general user `CAST(x AS ty)` now structures (retiring `canonical.rs`). The target type is the
    // canonical representative of each dialect's spelling, so a cast to a type that is its OWN
    // representative on all three dialects round-trips exactly: `I64` (bigint / SIGNED / INTEGER) and
    // `F64` (double precision / DOUBLE / REAL). A narrower authored type (e.g. `I32`, which renders
    // MySQL `SIGNED` and reads back `I64`) is deliberately NOT exercised here — it does not survive an
    // exact structural round-trip, exactly as an introspected MySQL/SQLite view pin does not (that
    // residual is why the check seam re-parses BOTH sides in the backend dialect rather than folding).
    cases.push((
        "cast-integer",
        cmp(
            CompareOp::Equals,
            ExprNode::Cast {
                operand: b(bare("amount")),
                ty: SqlType::I64,
            },
            lit("0"),
        ),
    ));
    cases.push((
        "cast-float",
        cmp(
            CompareOp::GreaterThan,
            ExprNode::Cast {
                operand: b(bare("ratio")),
                ty: SqlType::F64,
            },
            lit("0"),
        ),
    ));

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

// ---- single-SELECT view-body round-trip -----------------------------------------------------------

/// Renders a view's `CREATE VIEW` for `dialect` (schema `public`, no `OR REPLACE`).
fn render_view(view: &ViewModel, dialect: &dyn squealy::Dialect) -> String {
    let mut buf = Vec::new();
    squealy::render_create_view(Some("public"), view, false, dialect, &mut buf)
        .expect("view renders");
    String::from_utf8(buf).expect("rendered SQL is valid UTF-8")
}

/// Whether the round-tripped body is expected to equal the original *model* on `backend` (beyond the
/// re-render identity, which always holds). Two backends lose structural equality for principled,
/// documented reasons — the SQL still re-renders byte-identically (the epic invariant), only the
/// stricter model comparison is affected:
///
/// - **SQLite** has no schemas: its renderer suppresses the `schema` qualifier, so the round-tripped
///   `FROM` recovers `schema: None` instead of the original `Some("public")`. Re-qualifying is the
///   SQLite backend PR's job; here every SQLite case is model-equality-exempt.
/// - **MySQL `view/temporal`**: MySQL's `CAST` vocabulary collapses every integer width to `SIGNED`, so
///   the `EXTRACT(YEAR …)` result-pin's exact type (`I32`) is unrecoverable — it inverts to the
///   canonical `I64` (which re-renders to `SIGNED`, so identity holds) but is not structurally equal.
///   (The `view/aggregate` `SUM` pin is `I64`, which *is* the canonical, so it stays model-equal.)
fn view_model_equality_expected(case: &str, backend: &str) -> bool {
    match backend {
        "postgres" => true,
        "mysql" => case != "view/temporal",
        _ => false, // sqlite: schema qualifier suppressed
    }
}

/// The view-body spine (PR 2.0): a neutral [`ViewModel`], rendered to each dialect and read back through
/// the [`Reader`], must (a) re-render byte-identically on every dialect — the epic invariant
/// `render(parse(render(m))) == render(m)` — and (b) lower to the *same* body model wherever the
/// dialect's cast vocabulary and identifier rules are lossless (see [`view_model_equality_expected`]).
#[test]
fn view_bodies_round_trip_through_each_dialect() {
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

    let corpus = corpus();
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for (case, model) in &corpus {
        // Only the view cases; the table/* cases are covered by the constraint spine.
        if !case.starts_with("view/") {
            continue;
        }
        let view = &model.schemas[0].views[0];

        for (backend, sql_dialect, dialect) in dialects {
            let rendered = render_view(view, dialect);
            let lowered = match Reader::new(sql_dialect).read_create_view(&rendered) {
                Ok(lowered) => lowered,
                Err(err) => {
                    failures.push(format!(
                        "{case} on {backend}: read failed: {err}\n  SQL: {rendered}"
                    ));
                    continue;
                }
            };

            // (a) Re-render identity on every dialect: reconstruct a view around the lowered body (reusing
            // the original name/comment/columns — the output types are not in the SQL text) and re-render.
            let round_tripped = ViewModel {
                name: view.name.clone(),
                comment: view.comment.clone(),
                columns: view.columns.clone(),
                query: lowered.clone(),
            };
            let re_rendered = render_view(&round_tripped, dialect);
            if re_rendered != rendered {
                failures.push(format!(
                    "{case} on {backend}: re-rendered SQL differs\n  first:  {rendered}\n  second: {re_rendered}"
                ));
            }

            // (b) Model equality where the dialect is lossless.
            if view_model_equality_expected(case, backend) && lowered != view.query {
                failures.push(format!(
                    "{case} on {backend}: lowered body differs from the original\n  SQL: {rendered}\n  got: {lowered:?}"
                ));
            }
            checked += 1;
        }
    }

    assert!(
        failures.is_empty(),
        "view round-trip failed for {} case(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    // Every view case × every backend was exercised (17 views × 3 backends).
    assert_eq!(checked, 17 * dialects.len(), "view coverage drifted");
}

/// A plain, tail-less recursive-CTE arm renders BARE. An arm carrying its own `ORDER BY`/`LIMIT`/`OFFSET`
/// can only be scoped by parenthesizing it, which is **dialect-specific**: PostgreSQL/MySQL render it
/// parenthesized, but SQLite (whose recursive-CTE grammar forbids a parenthesized arm) rejects it — see
/// [`Dialect::supports_parenthesized_recursive_cte_arm`].
#[test]
fn a_recursive_cte_arm_with_a_tail_is_dialect_specific() {
    // `WITH RECURSIVE counter(n) AS (SELECT 1 LIMIT 1 UNION ALL SELECT counter.n + 1 FROM counter …)` —
    // the anchor carries a per-arm `LIMIT`, which cannot be scoped under a bare arm.
    let view = view_of(
        "v_bad",
        vec![("n", SqlType::I32)],
        ViewBody::With {
            recursive: true,
            ctes: vec![CteModel {
                name: "counter".to_owned(),
                columns: vec!["n".to_owned()],
                body: ViewBody::Set {
                    op: ViewSetOp::Union,
                    all: true,
                    left: Box::new(sel(ViewQueryModel {
                        projection: vec![proj("n", lit("1"))],
                        // A per-arm LIMIT on the anchor — unscopable under a bare recursive arm.
                        limit: Some(1),
                        ..ViewQueryModel::default()
                    })),
                    right: Box::new(sel(ViewQueryModel {
                        projection: vec![proj(
                            "n",
                            ExprNode::Binary {
                                op: ArithmeticOp::Add,
                                left: b(ExprNode::Column {
                                    alias: "q0_0".to_owned(),
                                    column: "n".to_owned(),
                                }),
                                right: b(lit("1")),
                            },
                        )],
                        from: Some(SourceItem::Named(SourceRef {
                            schema: None,
                            name: "counter".to_owned(),
                            alias: "q0_0".to_owned(),
                        })),
                        ..ViewQueryModel::default()
                    })),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                },
            }],
            body: Box::new(sel(ViewQueryModel {
                projection: vec![proj(
                    "n",
                    ExprNode::Column {
                        alias: "q0_0".to_owned(),
                        column: "n".to_owned(),
                    },
                )],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: "counter".to_owned(),
                    alias: "q0_0".to_owned(),
                })),
                ..ViewQueryModel::default()
            })),
        },
    );
    // PostgreSQL/MySQL parenthesize the scoped anchor — `(SELECT 1 LIMIT 1) UNION ALL …` — so the
    // per-arm LIMIT binds to the anchor, not the whole UNION.
    for dialect in [
        &squealy_postgresql::Postgres.dialect() as &dyn squealy::Dialect,
        &squealy_mysql::Mysql.dialect(),
    ] {
        let mut buf = Vec::new();
        squealy::render_create_view(Some("public"), &view, false, dialect, &mut buf)
            .expect("PostgreSQL/MySQL render a scoped recursive CTE arm parenthesized");
        let sql = String::from_utf8(buf).expect("utf-8 DDL");
        assert!(
            sql.contains("LIMIT 1) UNION ALL "),
            "the scoped anchor must be parenthesized: {sql}"
        );
    }
    // SQLite forbids a parenthesized recursive arm, so it has no valid rendering there — rejected.
    let mut buf = Vec::new();
    let result = squealy::render_create_view(
        Some("public"),
        &view,
        false,
        &squealy_sqlite::Sqlite.dialect(),
        &mut buf,
    );
    assert!(
        result.is_err(),
        "SQLite must reject a scoped recursive CTE arm (no valid rendering)"
    );
}

/// A recursive CTE must connect its arms with `UNION`/`UNION ALL`; an `INTERSECT`/`EXCEPT` over a
/// self-reference is not a valid recursive CTE (SQLite rejects it as a circular reference), so it is
/// rejected at render rather than emitting invalid DDL.
#[test]
fn a_recursive_cte_with_a_non_union_operator_is_rejected() {
    let counter_col = || ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: "n".to_owned(),
    };
    let view = view_of(
        "v_bad_op",
        vec![("n", SqlType::I32)],
        ViewBody::With {
            recursive: true,
            ctes: vec![CteModel {
                name: "counter".to_owned(),
                columns: vec!["n".to_owned()],
                body: ViewBody::Set {
                    // INTERSECT over a self-referential recursive term — not a valid recursive CTE.
                    op: ViewSetOp::Intersect,
                    all: false,
                    left: Box::new(sel(ViewQueryModel {
                        projection: vec![proj("n", lit("1"))],
                        ..ViewQueryModel::default()
                    })),
                    right: Box::new(sel(ViewQueryModel {
                        projection: vec![proj(
                            "n",
                            ExprNode::Binary {
                                op: ArithmeticOp::Add,
                                left: b(counter_col()),
                                right: b(lit("1")),
                            },
                        )],
                        from: Some(SourceItem::Named(SourceRef {
                            schema: None,
                            name: "counter".to_owned(),
                            alias: "q0_0".to_owned(),
                        })),
                        ..ViewQueryModel::default()
                    })),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                },
            }],
            body: Box::new(sel(ViewQueryModel {
                projection: vec![proj("n", counter_col())],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: "counter".to_owned(),
                    alias: "q0_0".to_owned(),
                })),
                ..ViewQueryModel::default()
            })),
        },
    );
    assert_view_render_rejected_on_all_dialects(
        &view,
        "a recursive CTE using INTERSECT/EXCEPT must be rejected, not rendered",
    );
}

/// A **non-recursive** `WITH` whose CTE shares its bare name with a schemaless relation its body reads is
/// NOT a recursive CTE: a CTE's own name is in scope in its body only under `WITH RECURSIVE`, so the body's
/// unqualified `counter` is an *outer* relation, not a self-reference. The clause-level `recursive` flag
/// gates self-reference detection, so this renders as a plain CTE body rather than being misclassified as
/// recursive (its body is a plain `SELECT`, not a `UNION`, which the recursive path would reject).
#[test]
fn a_plain_cte_shadowing_a_relation_name_is_not_recursive() {
    let counter_col = ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: "n".to_owned(),
    };
    let counter_source = || {
        Some(SourceItem::Named(SourceRef {
            schema: None,
            name: "counter".to_owned(),
            alias: "q0_0".to_owned(),
        }))
    };
    let view = view_of(
        "v_shadow",
        vec![("n", SqlType::I32)],
        ViewBody::With {
            recursive: false,
            ctes: vec![CteModel {
                name: "counter".to_owned(),
                columns: vec!["n".to_owned()],
                body: sel(ViewQueryModel {
                    projection: vec![proj("n", counter_col.clone())],
                    from: counter_source(),
                    ..ViewQueryModel::default()
                }),
            }],
            body: Box::new(sel(ViewQueryModel {
                projection: vec![proj("n", counter_col)],
                from: counter_source(),
                ..ViewQueryModel::default()
            })),
        },
    );
    for dialect in [
        &squealy_postgresql::Postgres.dialect() as &dyn squealy::Dialect,
        &squealy_mysql::Mysql.dialect(),
        &squealy_sqlite::Sqlite.dialect(),
    ] {
        let mut buf = Vec::new();
        squealy::render_create_view(Some("public"), &view, false, dialect, &mut buf)
            .expect("a plain CTE shadowing a relation name renders as a plain body, not recursive");
        let sql = String::from_utf8(buf).expect("utf-8 DDL");
        assert!(
            !sql.contains("RECURSIVE"),
            "a plain WITH must not become WITH RECURSIVE: {sql}"
        );
    }
}

/// Asserts `render_create_view` fails for `view` on all three dialects.
fn assert_view_render_rejected_on_all_dialects(view: &ViewModel, message: &str) {
    let dialects: [&dyn squealy::Dialect; 3] = [
        &squealy_postgresql::Postgres.dialect(),
        &squealy_mysql::Mysql.dialect(),
        &squealy_sqlite::Sqlite.dialect(),
    ];
    for dialect in dialects {
        let mut buf = Vec::new();
        let result = squealy::render_create_view(Some("public"), view, false, dialect, &mut buf);
        assert!(result.is_err(), "{message}");
    }
}
