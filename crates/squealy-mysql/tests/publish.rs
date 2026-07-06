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

#[allow(dead_code)]
#[derive(Schema)]
struct Catalog {
    widgets: Widget<'static, ColumnName>,
    parts: Part<'static, ColumnName>,
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

fn alter_column_baseline_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_alter".to_owned()),
            views: Vec::new(),
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
                    },
                ],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    }
}

fn alter_column_desired_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_alter".to_owned()),
            views: Vec::new(),
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
                    },
                ],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    }
}

fn mysql_normalized_catalog_schema() -> SchemaModel {
    SchemaModel {
        name: Some("catalog".to_owned()),
        views: Vec::new(),
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
                    },
                ],
                primary_key: Some(Constraint {
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
                    predicate: None,
                }],
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
                    },
                ],
                primary_key: Some(Constraint {
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: vec![Constraint {
                    name: "uq_widgets_name".to_owned(),
                    columns: vec!["name".to_owned()],
                }],
                checks: Vec::new(),
                indexes: Vec::new(),
            },
        ],
    }
}

fn rich_mysql_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog_rich".to_owned()),
            views: Vec::new(),
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
                        },
                    ],
                    primary_key: Some(Constraint {
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
                        expression: "quota > 0".to_owned(),
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
                        predicate: None,
                    }],
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
                                expression: "char_length(`slug`)".to_owned(),
                                storage: GeneratedStorage::Virtual,
                            }),
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
                        },
                    ],
                    primary_key: Some(Constraint {
                        name: "pk_tenants".to_owned(),
                        columns: vec!["id".to_owned()],
                    }),
                    foreign_keys: Vec::new(),
                    uniques: vec![Constraint {
                        name: "uq_tenants_slug".to_owned(),
                        columns: vec!["slug".to_owned()],
                    }],
                    checks: Vec::new(),
                    indexes: Vec::new(),
                },
            ],
        }],
    }
}

fn mysql_normalized_rich_schema() -> SchemaModel {
    SchemaModel {
        name: Some("catalog_rich".to_owned()),
        views: Vec::new(),
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
                    },
                ],
                primary_key: Some(Constraint {
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
                    expression: "(`quota` > 0)".to_owned(),
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
                    predicate: None,
                }],
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
                            expression: "char_length(`slug`)".to_owned(),
                            storage: GeneratedStorage::Virtual,
                        }),
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
                    },
                ],
                primary_key: Some(Constraint {
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: vec![Constraint {
                    name: "uq_tenants_slug".to_owned(),
                    columns: vec!["slug".to_owned()],
                }],
                checks: Vec::new(),
                indexes: Vec::new(),
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
                    },
                ],
                primary_key: Some(Constraint {
                    name: "PRIMARY".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    }
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
