//! Live end-to-end test: render create-from-scratch and execute it against MySQL.
//!
//! `#[ignore]`d like the other backend integration tests; run with a database via:
//! `SQUEALY_MYSQL_URL=... cargo test -p squealy-mysql --test publish -- --ignored`.

use std::sync::OnceLock;

use squealy::*;
use squealy_mysql::Mysql;
use tokio::sync::Mutex;

/// Serializes the live-database tests in this binary. They share one MySQL database, and the
/// incremental test introspects the *whole* database, so two tests running concurrently would see
/// each other's schemas. Each test holds this guard for its duration.
fn db_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    name: C::Type<'scope, String>,
    seats: C::Type<'scope, u32>,
}

// A referencing table so the live test exercises FK creation (the `ALTER … ADD CONSTRAINT` MySQL is
// strict about). `widget_id` matches `Widget::id` in size and sign.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Part<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(index, references(Widget::id, on_delete = "cascade"))]
    widget_id: C::Type<'scope, i32>,
}

// A single-`SELECT` view over the table. Its body — projection, `WHERE`, and `FROM` — is exactly the
// shape `read_view_query` reconstructs from MySQL's fully-qualified, alias-preserving `VIEW_DEFINITION`.
#[allow(dead_code)]
#[derive(View)]
#[schema(Catalog)]
struct ActiveWidget<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for ActiveWidget<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<Widget>()
            .where_(|widget| widget.seats.greater_than(0u32))
            .project(|(widget,)| (widget.id, widget.name))
    }
}

// A grouped/aggregated single-source view: `SELECT seats, count(id) … GROUP BY seats`. Its `count(id)`
// carries a result pin (which MySQL casts to `SIGNED` and the reverse parser inverts to a canonical
// `I64`), so this exercises the result-pin reconciliation end to end.
#[allow(dead_code)]
#[derive(View)]
#[schema(Catalog)]
struct WidgetSeatCount<'scope, C: ColumnMode = ColumnExpr> {
    seats: C::Type<'scope, u32>,
    count: C::Type<'scope, i64>,
}

impl<'scope, C: ColumnMode> ViewDefinition for WidgetSeatCount<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<Widget>()
            .group_by(|(widget,)| widget.seats)
            .project(|(widget,)| (widget.seats, widget.id.count()))
    }
}

#[allow(dead_code)]
#[derive(Schema)]
struct Catalog {
    widgets: Widget<'static, ColumnName>,
    parts: Part<'static, ColumnName>,
    #[view]
    active_widgets: ActiveWidget<'static, ColumnName>,
    #[view]
    widget_seat_counts: WidgetSeatCount<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct CatalogDb {
    catalog: Catalog,
}

fn database_url() -> String {
    std::env::var("SQUEALY_MYSQL_URL")
        .unwrap_or_else(|_| "mysql://root:root@127.0.0.1:33306/squealy_test".to_owned())
}

fn check_expr(sql: &str) -> squealy::ExprNode {
    squealy_parse::Reader::new(squealy_parse::SqlDialect::Mysql)
        .read_check_expression(sql)
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn publishes_create_from_scratch() {
    let _db_guard = db_lock().lock().await;
    let model = DatabaseModel::from_database::<CatalogDb>();
    let mut sql = Vec::new();
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");

    // Clean slate — the `catalog` schema is a MySQL database. (Re-runnable: render emits
    // CREATE TABLE, not IF NOT EXISTS.)
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog`")
        .await
        .expect("drop schema");

    // The whole script applies.
    connection
        .execute_ddl(&sql)
        .await
        .expect("create-from-scratch");

    // Re-running must fail because the objects now exist — proof they were created.
    assert!(
        connection.execute_ddl(&sql).await.is_err(),
        "re-running create-from-scratch should fail: objects already exist"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn publish_then_introspect_round_trips_mysql_schema_shape() {
    let _db_guard = db_lock().lock().await;
    let model = DatabaseModel::from_database::<CatalogDb>();
    let mut sql = Vec::new();
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog`")
        .await
        .expect("drop schema");

    connection
        .execute_ddl(&sql)
        .await
        .expect("create-from-scratch");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect published schema");
    let actual_schema = actual
        .schemas
        .into_iter()
        .find(|schema| schema.name.as_deref() == Some("catalog"))
        .expect("published schema should be introspected");

    assert_eq!(actual_schema, mysql_normalized_catalog_schema());

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn publish_then_introspect_preserves_richer_mysql_schema_facts() {
    let _db_guard = db_lock().lock().await;
    let model = rich_mysql_model();
    let mut sql = Vec::new();
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_rich`")
        .await
        .expect("drop schema");

    connection
        .execute_ddl(&sql)
        .await
        .expect("create rich schema");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect rich schema");
    let actual_schema = actual
        .schemas
        .into_iter()
        .find(|schema| schema.name.as_deref() == Some("catalog_rich"))
        .expect("rich schema should be introspected");

    assert_eq!(actual_schema, mysql_normalized_rich_schema());

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_rich`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn incremental_publish_applies_changed_column_definitions() {
    let _db_guard = db_lock().lock().await;
    let baseline = alter_column_baseline_model();
    let desired = alter_column_desired_model();
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_alter`")
        .await
        .expect("drop schema");

    squealy_model::publish(&baseline, &Mysql, &mut connection)
        .await
        .expect("publish baseline schema");

    let plan = squealy_model::plan_from_database(
        &desired,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("plan changed columns");
    assert_eq!(plan.steps.len(), 2);

    squealy_model::apply_plan(&plan, &desired, &Mysql, &mut connection)
        .await
        .expect("apply changed columns");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect altered schema");
    let actual_schema = actual
        .schemas
        .into_iter()
        .find(|schema| schema.name.as_deref() == Some("catalog_alter"))
        .expect("altered schema should be introspected");

    assert_eq!(actual_schema, desired.schemas[0]);

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_alter`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn replan_after_publish_is_empty() {
    let _db_guard = db_lock().lock().await;
    let model = DatabaseModel::from_database::<CatalogDb>();
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog`")
        .await
        .expect("drop schema");

    squealy_model::publish(&model, &Mysql, &mut connection)
        .await
        .expect("publish create-from-scratch");

    // Re-planning the same crate model against the freshly published schema must converge to an
    // empty plan. The crate model carries `auto_increment` identity as `ByDefault` and plain indexes
    // with no method/directions, while MySQL introspects `AutoIncrement` and `BTREE`/ASC; without
    // canonicalization these churn as never-settling AlterColumn/AlterIndex steps forever.
    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan against published schema");
    assert!(
        plan.steps.is_empty(),
        "expected empty plan after publish, got: {:?}",
        plan.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog`")
        .await
        .expect("cleanup");
}

/// A hand-built model for a view whose `ORDER BY` names a **projection output alias** — a shape the typed
/// `#[view]` builder cannot express. MySQL's `VIEW_DEFINITION` deparses the standalone `ORDER BY total` to
/// the underlying expression (`order by (q0_0.amount * 2)`), and the source table carries a colliding
/// `total` column (a standalone alias still wins). The clause-alias canonicalizer (git-bug 823ae69) must
/// converge the two so the re-plan is empty.
fn clause_alias_mysql_model() -> DatabaseModel {
    fn column(name: &str) -> ColumnModel {
        ColumnModel {
            name: name.to_owned(),
            comment: None,
            ty: SqlType::I64,
            collation: None,
            nullable: true,
            default: None,
            identity: None,
            generated: None,
            on_update: None,
        }
    }
    let amount_times_two = ExprNode::Binary {
        op: ArithmeticOp::Multiply,
        left: Box::new(ExprNode::Column {
            alias: "q0_0".to_owned(),
            column: "amount".to_owned(),
        }),
        right: Box::new(ExprNode::Literal("2".to_owned())),
    };
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("ca_mysql".to_owned()),
            tables: vec![TableModel {
                name: "ca_events".to_owned(),
                comment: None,
                columns: vec![column("amount"), column("total")],
                primary_key: None,
                foreign_keys: vec![],
                uniques: vec![],
                checks: vec![],
                indexes: vec![],
                exclusions: Vec::new(),
            }],
            views: vec![ViewModel {
                name: "ca_v".to_owned(),
                comment: None,
                columns: vec![ViewColumnModel {
                    name: "total".to_owned(),
                    ty: SqlType::I64,
                    nullable: true,
                }],
                query: ViewBody::Select(Box::new(ViewQueryModel {
                    projection: vec![ProjectionItem {
                        output_name: "total".to_owned(),
                        internal_alias: Some("total".to_owned()),
                        expr: amount_times_two,
                    }],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: Some("ca_mysql".to_owned()),
                        name: "ca_events".to_owned(),
                        alias: "q0_0".to_owned(),
                    })),
                    order_by: vec![OrderItem {
                        expr: ExprNode::BareColumn {
                            column: "total".to_owned(),
                        },
                        direction: None,
                        nulls: None,
                    }],
                    ..ViewQueryModel::default()
                })),
                materialized: false,
            }],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    }
}

#[tokio::test]
#[ignore]
async fn publishing_a_clause_alias_view_then_replanning_is_empty() {
    let _db_guard = db_lock().lock().await;
    let model = clause_alias_mysql_model();
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `ca_mysql`")
        .await
        .expect("drop schema");

    squealy_model::publish(&model, &Mysql, &mut connection)
        .await
        .expect("publish create-from-scratch");

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan against published schema");
    assert!(
        plan.steps.is_empty(),
        "expected empty plan after publishing a clause-alias view, got: {:?}",
        plan.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `ca_mysql`")
        .await
        .expect("cleanup");
}

fn alter_column_baseline_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_alter".to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![TableModel {
                name: "events".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "description".to_owned(),
                        comment: Some("Old description".to_owned()),
                        ty: SqlType::Varchar(255),
                        collation: None,
                        nullable: true,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "status".to_owned(),
                        comment: Some("Event status".to_owned()),
                        ty: SqlType::Varchar(64),
                        collation: None,
                        nullable: false,
                        default: Some(DefaultValue::Text("draft".to_owned())),
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                ],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
                exclusions: Vec::new(),
            }],
        }],
    }
}

fn alter_column_desired_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_alter".to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![TableModel {
                name: "events".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "description".to_owned(),
                        comment: Some("New description".to_owned()),
                        ty: SqlType::Varchar(128),
                        collation: None,
                        nullable: false,
                        default: Some(DefaultValue::Text("new".to_owned())),
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "status".to_owned(),
                        comment: None,
                        ty: SqlType::Varchar(64),
                        collation: None,
                        nullable: true,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                ],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
                exclusions: Vec::new(),
            }],
        }],
    }
}

fn mysql_normalized_catalog_schema() -> SchemaModel {
    SchemaModel {
        name: Some("catalog".to_owned()),
        views: vec![
            // A filtered/projected view. MySQL's `VIEW_DEFINITION` deparse is fully qualified and
            // alias-preserving, so the reverse parser reconstructs the exact structural body.
            ViewModel {
                name: "active_widgets".to_owned(),
                comment: None,
                columns: vec![
                    ViewColumnModel {
                        name: "id".to_owned(),
                        ty: SqlType::I32,
                        nullable: false,
                    },
                    ViewColumnModel {
                        name: "name".to_owned(),
                        ty: SqlType::Varchar(255),
                        nullable: false,
                    },
                ],
                query: ViewBody::Select(Box::new(ViewQueryModel {
                    projection: vec![
                        ProjectionItem {
                            output_name: "id".to_owned(),
                            internal_alias: None,
                            expr: ExprNode::Column {
                                alias: "q0_0".to_owned(),
                                column: "id".to_owned(),
                            },
                        },
                        ProjectionItem {
                            output_name: "name".to_owned(),
                            internal_alias: None,
                            expr: ExprNode::Column {
                                alias: "q0_0".to_owned(),
                                column: "name".to_owned(),
                            },
                        },
                    ],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: Some("catalog".to_owned()),
                        name: "widgets".to_owned(),
                        alias: "q0_0".to_owned(),
                    })),
                    filter: Some(ExprNode::Compare {
                        op: CompareOp::GreaterThan,
                        left: Box::new(ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "seats".to_owned(),
                        }),
                        right: Box::new(ExprNode::Literal("0".to_owned())),
                    }),
                    ..ViewQueryModel::default()
                })),
                materialized: false,
            },
            // A grouped/aggregated view: `count(id)` deparses without a result-pin cast (an unpinned
            // `count`), and its select-list alias is re-derived from the declared column name.
            ViewModel {
                name: "widget_seat_counts".to_owned(),
                comment: None,
                columns: vec![
                    ViewColumnModel {
                        name: "seats".to_owned(),
                        ty: SqlType::U32,
                        nullable: false,
                    },
                    ViewColumnModel {
                        name: "count".to_owned(),
                        ty: SqlType::I64,
                        nullable: false,
                    },
                ],
                query: ViewBody::Select(Box::new(ViewQueryModel {
                    projection: vec![
                        ProjectionItem {
                            output_name: "seats".to_owned(),
                            internal_alias: None,
                            expr: ExprNode::Column {
                                alias: "q0_0".to_owned(),
                                column: "seats".to_owned(),
                            },
                        },
                        ProjectionItem {
                            output_name: "count".to_owned(),
                            internal_alias: None,
                            expr: ExprNode::Aggregate {
                                func: AggregateFunc::Count,
                                distinct: false,
                                operand: Box::new(ExprNode::Column {
                                    alias: "q0_0".to_owned(),
                                    column: "id".to_owned(),
                                }),
                                result: None,
                            },
                        },
                    ],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: Some("catalog".to_owned()),
                        name: "widgets".to_owned(),
                        alias: "q0_0".to_owned(),
                    })),
                    group_by: vec![ExprNode::Column {
                        alias: "q0_0".to_owned(),
                        column: "seats".to_owned(),
                    }],
                    ..ViewQueryModel::default()
                })),
                materialized: false,
            },
        ],
        tables: vec![
            TableModel {
                name: "parts".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: Some(IdentityModel {
                            mode: IdentityMode::AutoIncrement,
                        }),
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "widget_id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                ],
                primary_key: Some(Constraint {
                    prefix_lengths: Vec::new(),
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: vec![ForeignKeyModel {
                    name: "fk_parts_widget_id".to_owned(),
                    columns: vec!["widget_id".to_owned()],
                    references_schema: Some("catalog".to_owned()),
                    references_table: "widgets".to_owned(),
                    references_columns: vec!["id".to_owned()],
                    match_type: None,
                    deferrability: None,
                    validation: None,
                    enforcement: None,
                    on_delete: Some(ForeignKeyAction::Cascade),
                    on_update: None,
                }],
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_parts_widget_id".to_owned(),
                    columns: vec!["widget_id".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Asc],
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    prefix_lengths: Vec::new(),
                    predicate: None,
                }],
                exclusions: Vec::new(),
            },
            TableModel {
                name: "widgets".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: Some(IdentityModel {
                            mode: IdentityMode::AutoIncrement,
                        }),
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "name".to_owned(),
                        comment: None,
                        ty: SqlType::Varchar(255),
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "seats".to_owned(),
                        comment: None,
                        ty: SqlType::U32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                ],
                primary_key: Some(Constraint {
                    prefix_lengths: Vec::new(),
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: vec![Constraint {
                    prefix_lengths: Vec::new(),
                    name: "uq_widgets_name".to_owned(),
                    columns: vec!["name".to_owned()],
                }],
                checks: Vec::new(),
                indexes: Vec::new(),
                exclusions: Vec::new(),
            },
        ],
        enums: Vec::new(),
        sequences: Vec::new(),
        domains: Vec::new(),
    }
}

fn rich_mysql_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_rich".to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![
                TableModel {
                    name: "memberships".to_owned(),
                    comment: Some("Tenant membership rows".to_owned()),
                    columns: vec![
                        ColumnModel {
                            name: "id".to_owned(),
                            comment: None,
                            ty: SqlType::I32,
                            collation: None,
                            nullable: false,
                            default: None,
                            identity: Some(IdentityModel {
                                mode: IdentityMode::AutoIncrement,
                            }),
                            generated: None,
                            on_update: None,
                        },
                        ColumnModel {
                            name: "tenant_id".to_owned(),
                            comment: Some("Referenced tenant id".to_owned()),
                            ty: SqlType::I32,
                            collation: None,
                            nullable: false,
                            default: None,
                            identity: None,
                            generated: None,
                            on_update: None,
                        },
                        ColumnModel {
                            name: "role_code".to_owned(),
                            comment: None,
                            ty: SqlType::Char(2),
                            collation: None,
                            nullable: false,
                            default: Some(DefaultValue::Text("MB".to_owned())),
                            identity: None,
                            generated: None,
                            on_update: None,
                        },
                        ColumnModel {
                            name: "quota".to_owned(),
                            comment: None,
                            ty: SqlType::Decimal {
                                precision: 10,
                                scale: 2,
                            },
                            collation: None,
                            nullable: false,
                            default: Some(DefaultValue::Raw("42.00".to_owned())),
                            identity: None,
                            generated: None,
                            on_update: None,
                        },
                        ColumnModel {
                            name: "active".to_owned(),
                            comment: None,
                            ty: SqlType::Bool,
                            collation: None,
                            nullable: false,
                            default: Some(DefaultValue::Bool(true)),
                            identity: None,
                            generated: None,
                            on_update: None,
                        },
                        // The MySQL `DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP` idiom
                        // (git-bug 7f4504d): the on-update attribute lives in `EXTRA` and must survive
                        // an introspection round-trip.
                        ColumnModel {
                            name: "updated_at".to_owned(),
                            comment: None,
                            ty: SqlType::Timestamp {
                                tz: true,
                                precision: None,
                            },
                            collation: None,
                            nullable: false,
                            default: Some(DefaultValue::CurrentTimestamp),
                            identity: None,
                            generated: None,
                            on_update: Some(Box::new(ExprNode::Now)),
                        },
                    ],
                    primary_key: Some(Constraint {
                        prefix_lengths: Vec::new(),
                        name: "pk_memberships".to_owned(),
                        columns: vec!["id".to_owned()],
                    }),
                    foreign_keys: vec![ForeignKeyModel {
                        name: "fk_memberships_tenant_id".to_owned(),
                        columns: vec!["tenant_id".to_owned()],
                        references_schema: Some("catalog_rich".to_owned()),
                        references_table: "tenants".to_owned(),
                        references_columns: vec!["id".to_owned()],
                        match_type: None,
                        deferrability: None,
                        validation: None,
                        enforcement: None,
                        on_delete: Some(ForeignKeyAction::Cascade),
                        on_update: None,
                    }],
                    uniques: Vec::new(),
                    checks: vec![CheckModel {
                        name: "ck_memberships_quota".to_owned(),
                        expression: check_expr("quota > 0"),
                        validation: None,
                        enforcement: None,
                    }],
                    indexes: vec![IndexModel {
                        name: "idx_memberships_tenant_id".to_owned(),
                        columns: vec!["tenant_id".to_owned()],
                        expressions: Vec::new(),
                        include_columns: Vec::new(),
                        unique: false,
                        method: Some(IndexMethod::BTree),
                        directions: Vec::new(),
                        nulls: Vec::new(),
                        collations: Vec::new(),
                        operator_classes: Vec::new(),
                        prefix_lengths: Vec::new(),
                        predicate: None,
                    }],
                    exclusions: Vec::new(),
                },
                TableModel {
                    name: "tenants".to_owned(),
                    comment: Some("Tenant catalog rows".to_owned()),
                    columns: vec![
                        ColumnModel {
                            name: "id".to_owned(),
                            comment: None,
                            ty: SqlType::I32,
                            collation: None,
                            nullable: false,
                            default: None,
                            identity: Some(IdentityModel {
                                mode: IdentityMode::AutoIncrement,
                            }),
                            generated: None,
                            on_update: None,
                        },
                        ColumnModel {
                            name: "slug".to_owned(),
                            comment: Some("Stable tenant slug".to_owned()),
                            ty: SqlType::Varchar(64),
                            collation: Some("utf8mb4_bin".to_owned()),
                            nullable: false,
                            default: None,
                            identity: None,
                            generated: None,
                            on_update: None,
                        },
                        ColumnModel {
                            name: "slug_len".to_owned(),
                            comment: None,
                            ty: SqlType::I32,
                            collation: None,
                            nullable: true,
                            default: None,
                            identity: None,
                            generated: Some(GeneratedColumnModel {
                                expression: Some(check_expr("char_length(`slug`)")),
                                storage: GeneratedStorage::Virtual,
                            }),
                            on_update: None,
                        },
                        ColumnModel {
                            name: "settings".to_owned(),
                            comment: None,
                            ty: SqlType::Json,
                            collation: None,
                            nullable: true,
                            default: None,
                            identity: None,
                            generated: None,
                            on_update: None,
                        },
                    ],
                    primary_key: Some(Constraint {
                        prefix_lengths: Vec::new(),
                        name: "pk_tenants".to_owned(),
                        columns: vec!["id".to_owned()],
                    }),
                    foreign_keys: Vec::new(),
                    uniques: vec![Constraint {
                        prefix_lengths: Vec::new(),
                        name: "uq_tenants_slug".to_owned(),
                        columns: vec!["slug".to_owned()],
                    }],
                    checks: Vec::new(),
                    indexes: Vec::new(),
                    exclusions: Vec::new(),
                },
            ],
        }],
    }
}

fn mysql_normalized_rich_schema() -> SchemaModel {
    SchemaModel {
        name: Some("catalog_rich".to_owned()),
        views: Vec::new(),
        enums: Vec::new(),
        sequences: Vec::new(),
        domains: Vec::new(),
        tables: vec![
            TableModel {
                name: "memberships".to_owned(),
                comment: Some("Tenant membership rows".to_owned()),
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: Some(IdentityModel {
                            mode: IdentityMode::AutoIncrement,
                        }),
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "tenant_id".to_owned(),
                        comment: Some("Referenced tenant id".to_owned()),
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "role_code".to_owned(),
                        comment: None,
                        ty: SqlType::Char(2),
                        collation: None,
                        nullable: false,
                        default: Some(DefaultValue::Text("MB".to_owned())),
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "quota".to_owned(),
                        comment: None,
                        ty: SqlType::Decimal {
                            precision: 10,
                            scale: 2,
                        },
                        collation: None,
                        nullable: false,
                        default: Some(DefaultValue::Raw("42.00".to_owned())),
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "active".to_owned(),
                        comment: None,
                        ty: SqlType::Bool,
                        collation: None,
                        nullable: false,
                        default: Some(DefaultValue::Bool(true)),
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                    // Introspected form of the `DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`
                    // column: a bare `TIMESTAMP` reads back as `tz: true`, fsp 0, and the on-update
                    // attribute is recovered from `EXTRA` as `Now`.
                    ColumnModel {
                        name: "updated_at".to_owned(),
                        comment: None,
                        ty: SqlType::Timestamp {
                            tz: true,
                            precision: Some(0),
                        },
                        collation: None,
                        nullable: false,
                        default: Some(DefaultValue::CurrentTimestamp),
                        identity: None,
                        generated: None,
                        on_update: Some(Box::new(ExprNode::Now)),
                    },
                ],
                primary_key: Some(Constraint {
                    prefix_lengths: Vec::new(),
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: vec![ForeignKeyModel {
                    name: "fk_memberships_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                    references_schema: Some("catalog_rich".to_owned()),
                    references_table: "tenants".to_owned(),
                    references_columns: vec!["id".to_owned()],
                    match_type: None,
                    deferrability: None,
                    validation: None,
                    enforcement: None,
                    on_delete: Some(ForeignKeyAction::Cascade),
                    on_update: None,
                }],
                uniques: Vec::new(),
                checks: vec![CheckModel {
                    name: "ck_memberships_quota".to_owned(),
                    expression: check_expr("(`quota` > 0)"),
                    validation: None,
                    enforcement: None,
                }],
                indexes: vec![IndexModel {
                    name: "idx_memberships_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Asc],
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    prefix_lengths: Vec::new(),
                    predicate: None,
                }],
                exclusions: Vec::new(),
            },
            TableModel {
                name: "tenants".to_owned(),
                comment: Some("Tenant catalog rows".to_owned()),
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: Some(IdentityModel {
                            mode: IdentityMode::AutoIncrement,
                        }),
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "slug".to_owned(),
                        comment: Some("Stable tenant slug".to_owned()),
                        ty: SqlType::Varchar(64),
                        collation: Some("utf8mb4_bin".to_owned()),
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "slug_len".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: true,
                        default: None,
                        identity: None,
                        generated: Some(GeneratedColumnModel {
                            expression: Some(check_expr("char_length(`slug`)")),
                            storage: GeneratedStorage::Virtual,
                        }),
                        on_update: None,
                    },
                    ColumnModel {
                        name: "settings".to_owned(),
                        comment: None,
                        ty: SqlType::Json,
                        collation: None,
                        nullable: true,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                ],
                primary_key: Some(Constraint {
                    prefix_lengths: Vec::new(),
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: vec![Constraint {
                    prefix_lengths: Vec::new(),
                    name: "uq_tenants_slug".to_owned(),
                    columns: vec!["slug".to_owned()],
                }],
                checks: Vec::new(),
                indexes: Vec::new(),
                exclusions: Vec::new(),
            },
        ],
    }
}

/// A one-table `catalog_ts` model whose `at` column is a `TIMESTAMP` at the given fractional-seconds
/// precision. Used to prove precision round-trips (churn-free) and that a precision change migrates.
fn timestamp_precision_model(precision: Option<u8>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_ts".to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![TableModel {
                name: "events".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: Some(IdentityModel {
                            mode: IdentityMode::ByDefault,
                        }),
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "at".to_owned(),
                        comment: None,
                        ty: SqlType::Timestamp {
                            tz: true,
                            precision,
                        },
                        collation: None,
                        nullable: false,
                        default: Some(DefaultValue::CurrentTimestamp),
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                ],
                primary_key: Some(Constraint {
                    prefix_lengths: Vec::new(),
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
                exclusions: Vec::new(),
            }],
        }],
    }
}

/// A one-table `catalog_dc` model whose table CHECK carries a general decimal cast
/// (`CAST(quota AS DECIMAL(10, scale)) > 0`) — the H2 shape from git-bug 8fe1530 (a decimal cast a
/// PostgreSQL package carries, deployed to MySQL). MySQL now renders `DECIMAL(p, s)` faithfully, so a
/// crate/package-authored decimal cast keeps its scale and round-trips.
fn decimal_cast_check_model(scale: u32) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_dc".to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![TableModel {
                name: "quotas".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: Some(IdentityModel {
                            mode: IdentityMode::ByDefault,
                        }),
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "quota".to_owned(),
                        comment: None,
                        ty: SqlType::Decimal {
                            precision: 10,
                            scale: 2,
                        },
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                ],
                primary_key: Some(Constraint {
                    prefix_lengths: Vec::new(),
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: vec![CheckModel {
                    name: "ck_quota_positive".to_owned(),
                    expression: check_expr(&format!("CAST(quota AS DECIMAL(10, {scale})) > 0")),
                    validation: None,
                    enforcement: None,
                }],
                indexes: Vec::new(),
                exclusions: Vec::new(),
            }],
        }],
    }
}

fn enforcement_check_model(enforcement: Option<ConstraintEnforcement>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_enf".to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![TableModel {
                name: "readings".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "n".to_owned(),
                    comment: None,
                    ty: SqlType::I32,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                    on_update: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: vec![CheckModel {
                    name: "ck_n_positive".to_owned(),
                    expression: check_expr("n > 0"),
                    validation: None,
                    enforcement,
                }],
                indexes: Vec::new(),
                exclusions: Vec::new(),
            }],
        }],
    }
}

#[tokio::test]
#[ignore]
async fn check_not_enforced_round_trips_and_toggling_enforcement_migrates() {
    let _db_guard = db_lock().lock().await;
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_enf`")
        .await
        .expect("drop schema");

    // Publish a `CHECK (...) NOT ENFORCED`, then re-plan: MySQL renders and reads back the enforcement
    // state structurally, so it re-plans empty (git-bug acb1c6d Phase 3, the enforcement finish).
    let not_enforced = enforcement_check_model(Some(ConstraintEnforcement::NotEnforced));
    squealy_model::publish(&not_enforced, &Mysql, &mut connection)
        .await
        .expect("publish NOT ENFORCED check");
    let replan = squealy_model::plan_from_database(
        &not_enforced,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan NOT ENFORCED check");
    assert!(
        replan.steps.is_empty(),
        "a NOT ENFORCED check must re-plan empty, got: {:?}",
        replan.steps
    );

    // Toggling to the enforced default must diff — a plain check (enforcement `None`) is a real change
    // from `NOT ENFORCED`, not folded away.
    let enforced = enforcement_check_model(None);
    let changed = squealy_model::plan_from_database(
        &enforced,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("plan enforcement toggle");
    assert!(
        !changed.steps.is_empty(),
        "toggling NOT ENFORCED -> enforced must diff"
    );

    // Apply the toggle, then confirm the now-enforced (default) check itself re-plans empty — the
    // enforced default folds to `None` on introspection, so it does not churn.
    squealy_model::apply_plan(&changed, &enforced, &Mysql, &mut connection)
        .await
        .expect("apply enforcement toggle");
    let replan_enforced = squealy_model::plan_from_database(
        &enforced,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan enforced check");
    assert!(
        replan_enforced.steps.is_empty(),
        "an enforced (default) check must re-plan empty, got: {:?}",
        replan_enforced.steps
    );

    // An explicitly-spelled `Some(Enforced)` (e.g. from a KDL package) must also re-plan empty against
    // the same live state — canonicalization folds the explicit default to `None`, matching what
    // introspection reads back, so it does not churn an endless `AlterCheck`.
    let explicit_enforced = enforcement_check_model(Some(ConstraintEnforcement::Enforced));
    let replan_explicit = squealy_model::plan_from_database(
        &explicit_enforced,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan explicit-enforced check");
    assert!(
        replan_explicit.steps.is_empty(),
        "an explicit Some(Enforced) check must re-plan empty, got: {:?}",
        replan_explicit.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_enf`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn enabling_enforcement_on_a_violating_check_fails_but_preserves_it() {
    let _db_guard = db_lock().lock().await;
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_enf`")
        .await
        .expect("drop schema");

    // Publish a NOT ENFORCED check, then insert a row that VIOLATES it — allowed precisely because the
    // check is not enforced. This is the realistic reason a check is NOT ENFORCED.
    let not_enforced = enforcement_check_model(Some(ConstraintEnforcement::NotEnforced));
    squealy_model::publish(&not_enforced, &Mysql, &mut connection)
        .await
        .expect("publish NOT ENFORCED check");
    connection
        .execute_ddl("INSERT INTO `catalog_enf`.`readings` (`n`) VALUES (-5)")
        .await
        .expect("insert a violating row");

    // Toggling to enforced must FAIL (the -5 row violates the check). The atomic `ALTER CHECK ...
    // ENFORCED` leaves the check intact on failure; a DROP + ADD would have committed the DROP and lost
    // the constraint entirely (codex round 2).
    let enforced = enforcement_check_model(None);
    let plan = squealy_model::plan_from_database(
        &enforced,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("plan enforcement enable");
    squealy_model::apply_plan(&plan, &enforced, &Mysql, &mut connection)
        .await
        .expect_err("enabling enforcement on a violating check must fail");

    // The check survives the failed migration — still present, still NOT ENFORCED.
    let after = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect after the failed toggle");
    let table = after
        .schemas
        .iter()
        .flat_map(|schema| &schema.tables)
        .find(|table| table.name == "readings")
        .expect("readings table still exists");
    let check = table
        .checks
        .iter()
        .find(|check| check.name == "ck_n_positive")
        .expect("the check must survive the failed enforcement toggle");
    assert_eq!(
        check.enforcement,
        Some(ConstraintEnforcement::NotEnforced),
        "the check must remain NOT ENFORCED after the failed toggle"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_enf`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn decimal_cast_check_round_trips_and_scale_change_migrates() {
    let _db_guard = db_lock().lock().await;
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_dc`")
        .await
        .expect("drop schema");

    // Publish a table CHECK carrying a general `CAST(quota AS DECIMAL(10, 2))`, then re-plan: MySQL
    // renders the precision/scale faithfully and reads it back structurally, so the cast re-plans empty
    // (no silent precision loss, no churn) — the cross-dialect deploy git-bug 8fe1530 fixes.
    let two = decimal_cast_check_model(2);
    squealy_model::publish(&two, &Mysql, &mut connection)
        .await
        .expect("publish decimal-cast check");
    let replan = squealy_model::plan_from_database(
        &two,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan decimal-cast check");
    assert!(
        replan.steps.is_empty(),
        "decimal-cast check must re-plan empty, got: {:?}",
        replan.steps
    );

    // A genuine scale change (`DECIMAL(10, 2)` -> `DECIMAL(10, 4)`) must diff — the canonical cast fold
    // preserves a general cast's scale rather than folding both sides to a bare representative.
    let four = decimal_cast_check_model(4);
    let changed = squealy_model::plan_from_database(
        &four,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("plan scale change");
    assert!(
        !changed.steps.is_empty(),
        "a decimal cast scale change (2 -> 4) must diff"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_dc`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn timestamp_precision_round_trips_and_migrates() {
    let _db_guard = db_lock().lock().await;
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_ts`")
        .await
        .expect("drop schema");

    // Publish a `TIMESTAMP(6)` column (with a matching `DEFAULT CURRENT_TIMESTAMP(6)`), then re-plan:
    // introspection must recover the precision so the replan is empty (no churn).
    let micros = timestamp_precision_model(Some(6));
    squealy_model::publish(&micros, &Mysql, &mut connection)
        .await
        .expect("publish timestamp(6)");
    let replan = squealy_model::plan_from_database(
        &micros,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan timestamp(6)");
    assert!(
        replan.steps.is_empty(),
        "timestamp(6) must re-plan empty, got: {:?}",
        replan.steps
    );

    // Narrowing the model to a bare `TIMESTAMP` (fsp 0) and back to `TIMESTAMP(6)` is a real precision
    // change: it must produce a column ALTER (auto-widening on publish), not a silent no-op.
    let seconds = timestamp_precision_model(None);
    let narrow = squealy_model::plan_from_database(
        &seconds,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("plan narrowing precision");
    assert!(
        !narrow.steps.is_empty(),
        "a precision change must diff (fsp 6 -> fsp 0)"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_ts`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn introspects_an_enum_column_and_a_functional_index_without_crashing() {
    // Guards two MySQL introspection defects: (1) a functional/expression index makes
    // `information_schema.STATISTICS.COLUMN_NAME` NULL, which must not abort introspection; (2) an
    // ENUM/SET column's member labels must keep their case (upper-casing them would change the allowed
    // values).
    let _db_guard = db_lock().lock().await;
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_enum`")
        .await
        .expect("drop schema");
    connection
        .execute_ddl("CREATE SCHEMA `catalog_enum`")
        .await
        .expect("create schema");
    connection
        .execute_ddl(
            "CREATE TABLE `catalog_enum`.`items` (\
`id` INT NOT NULL PRIMARY KEY, \
`status` ENUM('Active','InActive') NOT NULL, \
`name` VARCHAR(255) NOT NULL)",
        )
        .await
        .expect("create table with an enum column");
    // A functional index — the shape that used to abort introspection.
    connection
        .execute_ddl("CREATE INDEX `items_lower_name` ON `catalog_enum`.`items` ((LOWER(`name`)))")
        .await
        .expect("create functional index");

    // The whole point: this must not error.
    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspection must not crash on a functional index");

    let table = actual
        .schemas
        .iter()
        .find(|schema| schema.name.as_deref() == Some("catalog_enum"))
        .and_then(|schema| schema.tables.iter().find(|table| table.name == "items"))
        .expect("items table should be introspected");
    let status = table
        .columns
        .iter()
        .find(|column| column.name == "status")
        .expect("status column");
    assert_eq!(
        status.ty,
        SqlType::Raw("ENUM('Active','InActive')".to_owned()),
        "enum member labels must keep their case",
    );
    // squealy does not model expression indexes yet, so the functional index is skipped (not
    // crashed on, not mismodeled); the introspected model therefore renders without being rejected.
    assert!(
        !table
            .indexes
            .iter()
            .any(|index| index.name == "items_lower_name"),
        "the functional index should be skipped, not introspected: {:?}",
        table.indexes,
    );
    let mut rendered = Vec::new();
    Mysql
        .render_create(&actual, &mut rendered)
        .expect("the introspected model must render (functional index skipped, not carried)");

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_enum`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn introspects_and_round_trips_an_index_column_prefix_length() {
    // A prefix index (`INDEX(name(10))`) records the indexed prefix length in
    // `information_schema.STATISTICS.SUB_PART`. Dropping it read the index back as a full-column index,
    // so re-rendering produced a wider index and the plan never converged. Introspection must recover
    // the prefix length and re-planning against the published schema must be empty.
    let _db_guard = db_lock().lock().await;
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_prefix`")
        .await
        .expect("drop schema");
    connection
        .execute_ddl("CREATE SCHEMA `catalog_prefix`")
        .await
        .expect("create schema");
    connection
        .execute_ddl(
            "CREATE TABLE `catalog_prefix`.`items` (\
`id` INT NOT NULL PRIMARY KEY, \
`name` VARCHAR(255) NOT NULL, \
INDEX `items_name_prefix` (`name`(10)))",
        )
        .await
        .expect("create table with a prefix index");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect prefix index");
    let index = actual
        .schemas
        .iter()
        .find(|schema| schema.name.as_deref() == Some("catalog_prefix"))
        .and_then(|schema| schema.tables.iter().find(|table| table.name == "items"))
        .and_then(|table| {
            table
                .indexes
                .iter()
                .find(|index| index.name == "items_name_prefix")
        })
        .expect("prefix index should be introspected");
    assert_eq!(
        index.prefix_lengths,
        vec![IndexPrefixLength {
            position: 0,
            length: 10,
        }],
        "the index prefix length must be recovered, got: {:?}",
        index,
    );

    // Re-planning the introspected model against the same database must converge to an empty plan —
    // the prefix index round-trips exactly.
    let plan = squealy_model::plan_from_database(
        &actual,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan against published schema");
    assert!(
        plan.steps.is_empty(),
        "expected empty plan for a round-tripped prefix index, got: {:?}",
        plan.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_prefix`")
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn introspects_and_round_trips_constraint_column_prefix_lengths() {
    // MySQL exposes a `UNIQUE`/`PRIMARY KEY` over a leading column prefix as a `UNIQUE`/`PRIMARY KEY`
    // row in `information_schema.TABLE_CONSTRAINTS`, with the prefix length recorded only in
    // `STATISTICS.SUB_PART`. `key_constraints` must recover it onto the neutral `Constraint`, else a
    // published unique/pk prefix reads back as a full-column constraint and never converges.
    let _db_guard = db_lock().lock().await;
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_cprefix`")
        .await
        .expect("drop schema");
    connection
        .execute_ddl("CREATE SCHEMA `catalog_cprefix`")
        .await
        .expect("create schema");
    connection
        .execute_ddl(
            "CREATE TABLE `catalog_cprefix`.`items` (\
`code` VARCHAR(64) NOT NULL, \
`name` VARCHAR(255) NOT NULL, \
`blob_key` VARBINARY(32) NOT NULL, \
PRIMARY KEY (`code`(8)), \
UNIQUE KEY `uq_items_name` (`name`(10)), \
UNIQUE KEY `uq_items_blob` (`blob_key`(8)))",
        )
        .await
        .expect("create table with prefix constraints");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect prefix constraints");
    let table = actual
        .schemas
        .iter()
        .find(|schema| schema.name.as_deref() == Some("catalog_cprefix"))
        .and_then(|schema| schema.tables.iter().find(|table| table.name == "items"))
        .expect("items table should be introspected");
    assert_eq!(
        table
            .primary_key
            .as_ref()
            .expect("a primary key")
            .prefix_lengths,
        vec![IndexPrefixLength {
            position: 0,
            length: 8,
        }],
        "the primary key prefix length must be recovered, got: {:?}",
        table.primary_key,
    );
    let unique = table
        .uniques
        .iter()
        .find(|unique| unique.name == "uq_items_name")
        .expect("the unique constraint");
    assert_eq!(
        unique.prefix_lengths,
        vec![IndexPrefixLength {
            position: 0,
            length: 10,
        }],
        "the unique constraint prefix length must be recovered, got: {unique:?}",
    );
    // A `VARBINARY(32)` column introspects as `Raw` — its prefix must round-trip too.
    let blob_unique = table
        .uniques
        .iter()
        .find(|unique| unique.name == "uq_items_blob")
        .expect("the varbinary unique constraint");
    assert_eq!(
        blob_unique.prefix_lengths,
        vec![IndexPrefixLength {
            position: 0,
            length: 8,
        }],
        "the varbinary unique constraint prefix length must be recovered, got: {blob_unique:?}",
    );

    // Re-planning the introspected model against the same database must converge to an empty plan.
    let plan = squealy_model::plan_from_database(
        &actual,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan against published schema");
    assert!(
        plan.steps.is_empty(),
        "expected empty plan for round-tripped prefix constraints, got: {:?}",
        plan.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog_cprefix`")
        .await
        .expect("cleanup");
}
