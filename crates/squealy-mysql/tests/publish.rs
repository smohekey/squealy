//! Live end-to-end test: render create-from-scratch and execute it against MySQL.
//!
//! `#[ignore]`d like the other backend integration tests; run with a database via:
//! `SQUEALY_MYSQL_URL=... cargo test -p squealy-mysql --test publish -- --ignored`.

use squealy::*;
use squealy_mysql::Mysql;

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

fn mysql_normalized_catalog_schema() -> SchemaModel {
    SchemaModel {
        name: Some("catalog".to_owned()),
        tables: vec![
            TableModel {
                name: "parts".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
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
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                    ColumnModel {
                        name: "seats".to_owned(),
                        comment: None,
                        ty: SqlType::U32,
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
            tables: vec![
                TableModel {
                    name: "memberships".to_owned(),
                    comment: Some("Tenant membership rows".to_owned()),
                    columns: vec![
                        ColumnModel {
                            name: "id".to_owned(),
                            comment: None,
                            ty: SqlType::I32,
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
                            nullable: false,
                            default: None,
                            identity: None,
                            generated: None,
                        },
                        ColumnModel {
                            name: "role_code".to_owned(),
                            comment: None,
                            ty: SqlType::Char(2),
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
                            nullable: false,
                            default: Some(DefaultValue::Raw("42.00".to_owned())),
                            identity: None,
                            generated: None,
                        },
                        ColumnModel {
                            name: "active".to_owned(),
                            comment: None,
                            ty: SqlType::Bool,
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
                        on_delete: Some(ForeignKeyAction::Cascade),
                        on_update: None,
                    }],
                    uniques: Vec::new(),
                    checks: vec![CheckModel {
                        name: "ck_memberships_quota".to_owned(),
                        expression: "quota > 0".to_owned(),
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
                            nullable: false,
                            default: None,
                            identity: None,
                            generated: None,
                        },
                        ColumnModel {
                            name: "slug_len".to_owned(),
                            comment: None,
                            ty: SqlType::I32,
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
        tables: vec![
            TableModel {
                name: "memberships".to_owned(),
                comment: Some("Tenant membership rows".to_owned()),
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
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
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                    ColumnModel {
                        name: "role_code".to_owned(),
                        comment: None,
                        ty: SqlType::Char(2),
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
                        nullable: false,
                        default: Some(DefaultValue::Raw("42.00".to_owned())),
                        identity: None,
                        generated: None,
                    },
                    ColumnModel {
                        name: "active".to_owned(),
                        comment: None,
                        ty: SqlType::Bool,
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
                    on_delete: Some(ForeignKeyAction::Cascade),
                    on_update: None,
                }],
                uniques: Vec::new(),
                checks: vec![CheckModel {
                    name: "ck_memberships_quota".to_owned(),
                    expression: "(`quota` > 0)".to_owned(),
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
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                    ColumnModel {
                        name: "slug_len".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
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
