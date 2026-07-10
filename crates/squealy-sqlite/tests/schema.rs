//! Live schema-management round-trips against an in-memory SQLite database.
//!
//! Unlike the MySQL/PostgreSQL schema tests (which need a server and are `#[ignore]`d), SQLite runs
//! in-process, so these execute in the normal `cargo test` run. They cover introspection (render →
//! introspect), the churn-free replan guarantee (introspect → diff → empty), and the backend-owned
//! bookkeeping stores.

use squealy::{
    AggregateFunc, CheckModel, ColumnExpr, ColumnMode, ColumnModel, ColumnName, CompareOp,
    Constraint, Database, DatabaseModel, DdlExecutor, ExprNode, ForeignKeyModel, IdentityMode,
    IdentityModel, IndexModel, ModelConn, ProjectionItem, QueryBuilder, Schema, SchemaConnect,
    SchemaMetadataStore, SchemaModel, SchemaPublishHistoryStore, SchemaRefactorStore, SchemaView,
    SourceItem, SourceQuery, SourceRef, SqlType, Table, TableModel, View, ViewBody,
    ViewColumnModel, ViewDefinition, ViewModel, ViewQueryModel, ViewSelect,
};
use squealy_sqlite::{Sqlite, SqliteConnection};

async fn connect() -> SqliteConnection {
    Sqlite.connect(":memory:").await.expect("open in-memory db")
}

fn check_expr(sql: &str) -> squealy::ExprNode {
    squealy_parse::Reader::new(squealy_parse::SqlDialect::Sqlite)
        .read_check_expression(sql)
        .unwrap()
}

// A derive model exercising every fact SQLite loses on the way back: an `AUTOINCREMENT` identity
// primary key, a named foreign key, a named multi-column unique, a named secondary index, and a
// `#[schema(App)]` namespace — all of which the canonicalizers must flatten for the replan to be empty.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    active: C::Type<'scope, bool>,
    bio: C::Type<'scope, Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(references(User::id, on_delete = "cascade"))]
    user_id: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
#[unique(columns = [organization_id, slug])]
struct Repository<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    organization_id: C::Type<'scope, i32>,
    #[column(index)]
    slug: C::Type<'scope, String>,
}

// A filtered/projected single-`SELECT` view over `users`. Its body is exactly the shape
// `read_create_view` reconstructs from SQLite's stored `CREATE VIEW` text — with the schema qualifier
// suppressed, so the reconstructed source is unqualified (`schema: None`) while the desired body carries
// `Some("app")`; `canonical_view_body` flattens both to `None`. The `active = true` filter renders as
// `"active" = TRUE` on SQLite (which recognizes the `TRUE` keyword), round-tripping to a `Literal("TRUE")`
// on both sides so it re-plans to empty.
#[allow(dead_code)]
#[derive(View)]
#[schema(App)]
struct ActiveUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for ActiveUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<User>()
            .where_(|user| user.active.equals(true))
            .project(|(user,)| (user.id, user.name))
    }
}

// A grouped/aggregated view: `SELECT active, count(id) … GROUP BY active`. `count(id)` renders without a
// result-pin cast (an unpinned aggregate, `result: None`), so this exercises a `GROUP BY` body and the
// aggregate reconstruction alongside the schema-qualifier flatten. (The pin fold itself is covered by
// `canonical_sqlite_pin_type`'s unit tests.)
#[allow(dead_code)]
#[derive(View)]
#[schema(App)]
struct UserCountByActive<'scope, C: ColumnMode = ColumnExpr> {
    active: C::Type<'scope, bool>,
    count: C::Type<'scope, i64>,
}

impl<'scope, C: ColumnMode> ViewDefinition for UserCountByActive<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<User>()
            .group_by(|(user,)| user.active)
            .project(|(user,)| (user.active, user.id.count()))
    }
}

#[allow(dead_code)]
#[derive(Schema)]
struct App {
    users: User<'static, ColumnName>,
    posts: Post<'static, ColumnName>,
    repositorys: Repository<'static, ColumnName>,
    #[view]
    active_users: ActiveUser<'static, ColumnName>,
    #[view]
    user_counts: UserCountByActive<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct AppDb {
    app: App,
}

#[tokio::test]
async fn replan_after_publish_is_empty() {
    let model = DatabaseModel::from_database::<AppDb>();
    let mut connection = connect().await;

    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish create-from-scratch");

    // Re-planning the crate model against the freshly published schema must converge to an empty plan.
    // The crate model carries schema-qualified, named constraints and `ByDefault` identity, while SQLite
    // introspects an unqualified schema, unnamed constraints and `AutoIncrement`; without the
    // canonicalizers these churn as never-settling steps forever.
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
}

#[tokio::test]
async fn introspects_the_published_schema_shape() {
    let model = DatabaseModel::from_database::<AppDb>();
    let mut connection = connect().await;
    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish create-from-scratch");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect published schema");

    assert_eq!(actual, expected_introspected_model());
}

/// The concrete shape introspection reads back for [`AppDb`] — a single unqualified schema, affinity
/// types (`i32` → `I64`), unnamed constraints, and `AutoIncrement` identity.
fn expected_introspected_model() -> DatabaseModel {
    let autoincrement_id = || ColumnModel {
        name: "id".to_owned(),
        comment: None,
        ty: SqlType::I64,
        collation: None,
        nullable: false,
        default: None,
        identity: Some(IdentityModel {
            mode: IdentityMode::AutoIncrement,
        }),
        generated: None,
    };
    let plain = |name: &str, ty: SqlType, nullable: bool| ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty,
        collation: None,
        nullable,
        default: None,
        identity: None,
        generated: None,
    };
    let pk = || {
        Some(Constraint {
            name: String::new(),
            columns: vec!["id".to_owned()],
        })
    };

    // A `q0_0`-aliased column reference on the sole `users` source, as the reverse parser reconstructs it.
    let user_col = |column: &str| ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: column.to_owned(),
    };
    let users_source = || {
        Some(SourceItem::Named(SourceRef {
            schema: None,
            name: "users".to_owned(),
            alias: "q0_0".to_owned(),
        }))
    };

    DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            // Views come back in `sqlite_master` name order: active_users, user_count_by_actives. SQLite
            // reconstructs each body from the stored `CREATE VIEW` text — with the schema qualifier
            // suppressed (source `schema: None`) and columns typed as the `Bytes` sentinel (SQLite cannot
            // type a view output). The `active = true` filter round-trips as `Literal("TRUE")`, and
            // `count(id)` deparses without a result-pin cast (`result: None`).
            views: vec![
                ViewModel {
                    name: "active_users".to_owned(),
                    comment: None,
                    columns: vec![
                        ViewColumnModel {
                            name: "id".to_owned(),
                            ty: SqlType::Bytes,
                            nullable: false,
                        },
                        ViewColumnModel {
                            name: "name".to_owned(),
                            ty: SqlType::Bytes,
                            nullable: false,
                        },
                    ],
                    query: ViewBody::Select(Box::new(ViewQueryModel {
                        projection: vec![
                            ProjectionItem {
                                output_name: "id".to_owned(),
                                expr: user_col("id"),
                            },
                            ProjectionItem {
                                output_name: "name".to_owned(),
                                expr: user_col("name"),
                            },
                        ],
                        from: users_source(),
                        filter: Some(ExprNode::Compare {
                            op: CompareOp::Equals,
                            left: Box::new(user_col("active")),
                            right: Box::new(ExprNode::Literal("TRUE".to_owned())),
                        }),
                        ..ViewQueryModel::default()
                    })),
                },
                ViewModel {
                    name: "user_count_by_actives".to_owned(),
                    comment: None,
                    columns: vec![
                        ViewColumnModel {
                            name: "active".to_owned(),
                            ty: SqlType::Bytes,
                            nullable: false,
                        },
                        ViewColumnModel {
                            name: "count".to_owned(),
                            ty: SqlType::Bytes,
                            nullable: false,
                        },
                    ],
                    query: ViewBody::Select(Box::new(ViewQueryModel {
                        projection: vec![
                            ProjectionItem {
                                output_name: "active".to_owned(),
                                expr: user_col("active"),
                            },
                            ProjectionItem {
                                output_name: "count".to_owned(),
                                expr: ExprNode::Aggregate {
                                    func: AggregateFunc::Count,
                                    distinct: false,
                                    operand: Box::new(user_col("id")),
                                    result: None,
                                },
                            },
                        ],
                        from: users_source(),
                        group_by: vec![user_col("active")],
                        ..ViewQueryModel::default()
                    })),
                },
            ],
            // Tables come back in `sqlite_master` name order: posts, repositorys, users.
            tables: vec![
                TableModel {
                    name: "posts".to_owned(),
                    comment: None,
                    columns: vec![autoincrement_id(), plain("user_id", SqlType::I64, false)],
                    primary_key: pk(),
                    foreign_keys: vec![ForeignKeyModel {
                        name: String::new(),
                        columns: vec!["user_id".to_owned()],
                        references_schema: None,
                        references_table: "users".to_owned(),
                        references_columns: vec!["id".to_owned()],
                        match_type: None,
                        deferrability: None,
                        validation: None,
                        enforcement: None,
                        on_delete: Some(squealy::ForeignKeyAction::Cascade),
                        on_update: None,
                    }],
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: Vec::new(),
                },
                TableModel {
                    name: "repositorys".to_owned(),
                    comment: None,
                    columns: vec![
                        autoincrement_id(),
                        plain("organization_id", SqlType::I64, false),
                        plain("slug", SqlType::Text, false),
                    ],
                    primary_key: pk(),
                    foreign_keys: Vec::new(),
                    uniques: vec![Constraint {
                        name: String::new(),
                        columns: vec!["organization_id".to_owned(), "slug".to_owned()],
                    }],
                    checks: Vec::new(),
                    indexes: vec![IndexModel {
                        name: "idx_repositorys_slug".to_owned(),
                        columns: vec!["slug".to_owned()],
                        expressions: Vec::new(),
                        include_columns: Vec::new(),
                        unique: false,
                        method: None,
                        directions: Vec::new(),
                        nulls: Vec::new(),
                        collations: Vec::new(),
                        operator_classes: Vec::new(),
                        predicate: None,
                    }],
                },
                TableModel {
                    name: "users".to_owned(),
                    comment: None,
                    columns: vec![
                        autoincrement_id(),
                        plain("name", SqlType::Text, false),
                        plain("active", SqlType::I64, false),
                        plain("bio", SqlType::Text, true),
                    ],
                    primary_key: pk(),
                    foreign_keys: Vec::new(),
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: Vec::new(),
                },
            ],
        }],
    }
}

#[tokio::test]
async fn round_trips_a_partial_index_predicate() {
    // A partial index's `WHERE` predicate is not reported by any PRAGMA; introspection recovers it from
    // the stored `CREATE INDEX` text, so it round-trips (and re-plans to an empty diff).
    let column = |name: &str, ty: SqlType, nullable: bool| ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty,
        collation: None,
        nullable,
        default: None,
        identity: None,
        generated: None,
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            tables: vec![TableModel {
                name: "docs".to_owned(),
                comment: None,
                columns: vec![
                    column("id", SqlType::I64, false),
                    column("deleted_at", SqlType::Text, true),
                ],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_docs_active".to_owned(),
                    columns: vec!["id".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: false,
                    method: None,
                    directions: Vec::new(),
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: Some(Box::new(squealy::ExprNode::IsNull {
                        negated: false,
                        operand: Box::new(squealy::ExprNode::BareColumn {
                            column: "deleted_at".to_owned(),
                        }),
                    })),
                }],
            }],
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish partial index");

    let actual = squealy_model::introspect(&mut connection).await.unwrap();
    let index = &actual.schemas[0].tables[0].indexes[0];
    assert_eq!(
        index.predicate,
        Some(Box::new(squealy::ExprNode::IsNull {
            negated: false,
            operand: Box::new(squealy::ExprNode::BareColumn {
                column: "deleted_at".to_owned(),
            }),
        }))
    );

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan partial index");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);
}

#[tokio::test]
async fn round_trips_bool_and_unsigned_defaults() {
    // SQLite has no boolean or unsigned literal: a `bool`/unsigned default renders as an integer and
    // reads back as `Int`. The default canonicalizer collapses the desired side the same way, so a
    // defaulted column re-plans to an empty diff instead of a never-settling `AlterColumn`.
    use squealy::DefaultValue;
    let column = |name: &str, ty: SqlType, default: DefaultValue| ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty,
        collation: None,
        nullable: false,
        default: Some(default),
        identity: None,
        generated: None,
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            tables: vec![TableModel {
                name: "flags".to_owned(),
                comment: None,
                columns: vec![
                    column("active", SqlType::Bool, DefaultValue::Bool(true)),
                    column("seats", SqlType::U32, DefaultValue::UInt(5)),
                    // A NUMERIC-affinity column: the structured `Int` default reads back as `Raw("0")`.
                    column(
                        "balance",
                        SqlType::Decimal {
                            precision: 10,
                            scale: 2,
                        },
                        DefaultValue::Int(0),
                    ),
                    // An unsigned default beyond `i128::MAX` reads back as `Raw(text)` (its `parse::<i128>`
                    // overflows), so canonicalization must produce the same raw decimal.
                    column(
                        "huge",
                        SqlType::U128,
                        DefaultValue::UInt((i128::MAX as u128) + 1),
                    ),
                ],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish defaults");

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan defaults");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);
}

#[tokio::test]
async fn coalesces_flattened_schemas_on_replan() {
    // A two-schema model publishes into SQLite's single namespace; canonicalization must coalesce the
    // flattened schemas so the re-plan does not drop the tables of all but one of them.
    let table = |name: &str| TableModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![ColumnModel {
            name: "id".to_owned(),
            comment: None,
            ty: SqlType::I64,
            collation: None,
            nullable: false,
            default: None,
            identity: None,
            generated: None,
        }],
        primary_key: None,
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
    };
    let model = DatabaseModel {
        schemas: vec![
            SchemaModel {
                name: Some("app".to_owned()),
                views: Vec::new(),
                tables: vec![table("users")],
            },
            SchemaModel {
                name: Some("archive".to_owned()),
                views: Vec::new(),
                tables: vec![table("logs")],
            },
        ],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish two schemas");

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan two schemas");
    assert!(
        plan.steps.is_empty(),
        "flattened schemas should not churn, got: {:?}",
        plan.steps
    );
}

#[tokio::test]
async fn resolves_foreign_key_with_omitted_parent_columns() {
    // A foreign key written `REFERENCES parent` (no column list) references the parent's primary key;
    // `PRAGMA foreign_key_list` reports NULL for the parent column, which introspection resolves to the
    // parent's primary key so a valid SQLite schema not created by this renderer still introspects.
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);\
             CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent)",
        )
        .await
        .expect("create tables");

    let actual = squealy_model::introspect(&mut connection).await.unwrap();
    let child = actual.schemas[0]
        .tables
        .iter()
        .find(|table| table.name == "child")
        .expect("child table");
    assert_eq!(child.foreign_keys.len(), 1);
    assert_eq!(child.foreign_keys[0].references_table, "parent");
    assert_eq!(
        child.foreign_keys[0].references_columns,
        vec!["id".to_owned()]
    );
}

#[tokio::test]
async fn honors_schema_qualified_table_rename_refactor() {
    use squealy::DatabasePlanStep;
    use squealy_model::{RefactorLog, RefactorOperation, RenameTable};

    let table = |name: &str| TableModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![ColumnModel {
            name: "label".to_owned(),
            comment: None,
            ty: SqlType::Text,
            collation: None,
            nullable: false,
            default: None,
            identity: None,
            generated: None,
        }],
        primary_key: None,
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
    };
    let model = |table_name: &str| DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            views: Vec::new(),
            tables: vec![table(table_name)],
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model("events_old"), &Sqlite, &mut connection)
        .await
        .expect("publish baseline");

    // The refactor carries the crate's schema (`Some("app")`); SQLite introspects the table under the
    // flattened (`None`) namespace, so without canonicalizing the refactor it would not match and the
    // plan would drop+recreate instead of renaming.
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameTable(RenameTable {
            id: "rename-events".to_owned(),
            schema: Some("app".to_owned()),
            from: "events_old".to_owned(),
            to: "events_new".to_owned(),
        })],
    };

    let plan = squealy_model::plan_from_database_with_refactors(
        &model("events_new"),
        &refactors,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("plan with rename refactor");

    assert!(
        plan.steps.iter().any(|step| matches!(
            step,
            DatabasePlanStep::RenameTable { from, to, .. } if from == "events_old" && to == "events_new"
        )),
        "expected a RenameTable step, got: {:?}",
        plan.steps
    );
    assert!(
        !plan
            .steps
            .iter()
            .any(|step| matches!(step, DatabasePlanStep::DropTable { .. })),
        "rename must not drop the table, got: {:?}",
        plan.steps
    );
}

#[tokio::test]
async fn round_trips_fixed_bytes_width() {
    // A `[u8; N]` column renders as BLOB + a generated width check; introspection recovers the width so
    // `FixedBytes(N)` round-trips (empty re-plan) and a size change still diffs.
    let model = |width: u32| DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            tables: vec![TableModel {
                name: "blobs".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "digest".to_owned(),
                    comment: None,
                    ty: SqlType::FixedBytes(width),
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model(16), &Sqlite, &mut connection)
        .await
        .expect("publish fixed bytes");

    let actual = squealy_model::introspect(&mut connection).await.unwrap();
    assert_eq!(
        actual.schemas[0].tables[0].columns[0].ty,
        SqlType::FixedBytes(16)
    );

    // Same width re-plans empty.
    let plan = squealy_model::plan_from_database(
        &model(16),
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan same width");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);

    // A different width is a real change (not silently equal).
    let plan = squealy_model::plan_from_database(
        &model(32),
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan different width");
    assert!(!plan.steps.is_empty(), "width change must diff");
}

#[tokio::test]
async fn preserves_nullability_of_non_rowid_primary_key_columns() {
    // Only a single-column INTEGER primary key (the rowid alias) is implicitly NOT NULL; a TEXT primary
    // key and a composite key allow NULLs unless declared NOT NULL, so introspection must not force them.
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE rowid_pk (id INTEGER PRIMARY KEY, label TEXT);\
             CREATE TABLE text_pk (slug TEXT PRIMARY KEY, label TEXT);\
             CREATE TABLE composite_pk (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        )
        .await
        .expect("create tables");

    let actual = squealy_model::introspect(&mut connection).await.unwrap();
    let column = |table: &str, name: &str| -> bool {
        actual.schemas[0]
            .tables
            .iter()
            .find(|t| t.name == table)
            .unwrap()
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap()
            .nullable
    };
    // The rowid alias is not nullable (table_info reports notnull=0, but SQLite enforces NOT NULL).
    assert!(!column("rowid_pk", "id"));
    // A TEXT primary key and composite-key columns genuinely allow NULLs.
    assert!(column("text_pk", "slug"));
    assert!(column("composite_pk", "a"));
    assert!(column("composite_pk", "b"));
}

#[tokio::test]
async fn round_trips_an_explicit_ascending_index_direction() {
    // A model that spells out an all-ascending index direction (`ASC`) must re-plan empty: SQLite
    // introspects an all-ascending index with empty directions, and canonicalization collapses the
    // explicit `Asc` list to match.
    let index = IndexModel {
        name: "idx_docs_slug".to_owned(),
        columns: vec!["slug".to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: vec![squealy::IndexDirection::Asc],
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        predicate: None,
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            tables: vec![TableModel {
                name: "docs".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "slug".to_owned(),
                    comment: None,
                    ty: SqlType::Text,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![index],
            }],
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish explicit ASC index");

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan explicit ASC index");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);
}

#[tokio::test]
async fn round_trips_a_partial_descending_index_direction() {
    // A multi-column index that specifies only a non-default prefix (`[Desc]` for two columns) renders
    // `"slug" DESC, "rank"` and reads back as `[Desc, Asc]`; trimming the trailing implicit `Asc` on
    // both sides makes it re-plan empty.
    let column = |name: &str| ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty: SqlType::Text,
        collation: None,
        nullable: false,
        default: None,
        identity: None,
        generated: None,
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            tables: vec![TableModel {
                name: "docs".to_owned(),
                comment: None,
                columns: vec![column("slug"), column("rank")],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_docs_slug_rank".to_owned(),
                    columns: vec!["slug".to_owned(), "rank".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: false,
                    method: None,
                    directions: vec![squealy::IndexDirection::Desc],
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: None,
                }],
            }],
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish partial-descending index");

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan partial-descending index");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);
}

#[tokio::test]
async fn replans_empty_for_a_desired_model_with_only_empty_schemas() {
    // A desired model whose (schema-qualified) namespace has no tables publishes no DDL, and the empty
    // database introspects to `schemas: []`. Canonicalization drops the empty flattened schema, so the
    // re-plan is clean rather than a spurious CreateSchema.
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            views: Vec::new(),
            tables: Vec::new(),
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish empty-schema model");

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan empty-schema model");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);
}

#[tokio::test]
async fn introspects_empty_database_as_no_schemas() {
    // SQLite has no namespace object, so an empty database introspects to `schemas: []` — not a phantom
    // default schema that would diff as a spurious DropSchema against an empty model.
    let mut connection = connect().await;
    let actual = squealy_model::introspect(&mut connection).await.unwrap();
    assert!(actual.schemas.is_empty(), "got: {:?}", actual.schemas);

    let plan = squealy_model::plan_from_database(
        &DatabaseModel { schemas: vec![] },
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan empty vs empty");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);
}

#[tokio::test]
async fn refactor_store_records_and_reads_ids() {
    let mut connection = connect().await;
    // A read before any write returns empty (the bookkeeping table does not exist yet).
    assert!(connection.applied_refactor_ids().await.unwrap().is_empty());

    connection
        .record_applied_refactor_ids(&["r2".to_owned(), "r1".to_owned()])
        .await
        .expect("record ids");
    // Re-recording an existing id is ignored (INSERT OR IGNORE), and a new id is added.
    connection
        .record_applied_refactor_ids(&["r1".to_owned(), "r3".to_owned()])
        .await
        .expect("record more ids");

    assert_eq!(
        connection.applied_refactor_ids().await.unwrap(),
        vec!["r1".to_owned(), "r2".to_owned(), "r3".to_owned()]
    );
}

#[tokio::test]
async fn metadata_store_upserts_by_name() {
    let mut connection = connect().await;
    assert!(connection.schema_metadata().await.unwrap().is_empty());

    connection
        .record_schema_metadata(&[
            ("package_hash".to_owned(), "abc".to_owned()),
            ("format".to_owned(), "1".to_owned()),
        ])
        .await
        .expect("record metadata");
    // Recording the same name replaces its value (ON CONFLICT DO UPDATE).
    connection
        .record_schema_metadata(&[("package_hash".to_owned(), "def".to_owned())])
        .await
        .expect("update metadata");

    assert_eq!(
        connection.schema_metadata().await.unwrap(),
        vec![
            ("format".to_owned(), "1".to_owned()),
            ("package_hash".to_owned(), "def".to_owned()),
        ]
    );
}

#[tokio::test]
async fn publish_history_store_appends_newest_first() {
    let mut connection = connect().await;
    assert!(
        connection
            .schema_publish_history(10)
            .await
            .unwrap()
            .is_empty()
    );

    connection
        .record_schema_publish("create", "hash1", "1")
        .await
        .expect("record first publish");
    connection
        .record_schema_publish("incremental", "hash2", "1")
        .await
        .expect("record second publish");

    let history = connection.schema_publish_history(10).await.unwrap();
    assert_eq!(history.len(), 2);
    // Newest first.
    assert_eq!(history[0].mode, "incremental");
    assert_eq!(history[0].package_hash, "hash2");
    assert_eq!(history[1].mode, "create");
    // `limit` caps the result.
    assert_eq!(connection.schema_publish_history(1).await.unwrap().len(), 1);
    // `limit == 0` returns nothing.
    assert!(
        connection
            .schema_publish_history(0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn round_trips_a_table_check_constraint() {
    // A table `CHECK` lives only in the `CREATE TABLE` text; introspection recovers the expression and
    // matches it by a name derived from that expression, so a checked table re-plans empty. A changed
    // expression is still a real diff.
    let model = |expression: &str| DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            tables: vec![TableModel {
                name: "accounts".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "balance".to_owned(),
                    comment: None,
                    ty: SqlType::I64,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: vec![CheckModel {
                    name: "ck_accounts_balance".to_owned(),
                    expression: check_expr(expression),
                    validation: None,
                    enforcement: None,
                }],
                indexes: Vec::new(),
            }],
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model("balance >= 0"), &Sqlite, &mut connection)
        .await
        .expect("publish checked table");

    // The check is read back with its expression (unnamed).
    let actual = squealy_model::introspect(&mut connection).await.unwrap();
    let checks = &actual.schemas[0].tables[0].checks;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].expression, check_expr("balance >= 0"));
    assert!(checks[0].name.is_empty());

    // Same expression re-plans empty.
    let plan = squealy_model::plan_from_database(
        &model("balance >= 0"),
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan same check");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);

    // A different expression is a real change (a table rebuild, since checks are inline-only).
    let plan = squealy_model::plan_from_database(
        &model("balance > 0"),
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan changed check");
    assert!(!plan.steps.is_empty(), "check change must diff");
}

#[tokio::test]
async fn round_trips_a_column_collation() {
    // A column `COLLATE` clause lives only in the `CREATE TABLE` text; introspection recovers it by
    // parsing that text, so a collated column re-plans empty. A changed collation is still a real diff.
    let model = |collation: &str| DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            tables: vec![TableModel {
                name: "people".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "name".to_owned(),
                    comment: None,
                    ty: SqlType::Text,
                    collation: Some(collation.to_owned()),
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    };

    let mut connection = connect().await;
    squealy_model::publish(&model("NOCASE"), &Sqlite, &mut connection)
        .await
        .expect("publish collated column");

    // The collation is read back.
    let actual = squealy_model::introspect(&mut connection).await.unwrap();
    assert_eq!(
        actual.schemas[0].tables[0].columns[0].collation,
        Some("NOCASE".to_owned())
    );

    // Same collation re-plans empty.
    let plan = squealy_model::plan_from_database(
        &model("NOCASE"),
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan same collation");
    assert!(plan.steps.is_empty(), "got: {:?}", plan.steps);

    // A different collation is a real change (a rebuild, since a column collation is inline-only).
    let plan = squealy_model::plan_from_database(
        &model("RTRIM"),
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan changed collation");
    assert!(!plan.steps.is_empty(), "collation change must diff");
}
