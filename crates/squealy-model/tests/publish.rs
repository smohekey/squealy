//! Live end-to-end test of `publish` (create-from-scratch executed against PostgreSQL).
//!
//! `#[ignore]`d like the other PostgreSQL integration tests; run with a database via:
//! `SQUEALY_POSTGRES_URL=... cargo test -p squealy-model --test publish -- --ignored`.

use squealy::*;
use squealy_postgresql::{Postgres, PostgresConnection};
use tokio_postgres::NoTls;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(PublishDemo)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    name: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct PublishDemo {
    widgets: Widget<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct PublishDemoDb {
    publish_demo: PublishDemo,
}

fn database_url() -> String {
    std::env::var("SQUEALY_POSTGRES_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:55432/squealy_test".to_owned())
}

async fn connect() -> PostgresConnection {
    let (client, connection) = tokio_postgres::connect(&database_url(), NoTls)
        .await
        .expect("connect to PostgreSQL");
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            panic!("PostgreSQL connection failed: {error}");
        }
    });
    PostgresConnection::new(client)
}

#[tokio::test]
#[ignore]
async fn publish_creates_schema_then_round_trips_rows() {
    let mut connection = connect().await;

    // Clean slate so the test is re-runnable (render_create emits CREATE TABLE, not IF NOT EXISTS).
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"publish_demo\" CASCADE")
        .await
        .expect("drop schema");

    squealy_model::publish_database::<PublishDemoDb, _, _>(&Postgres, &mut connection)
        .await
        .expect("publish create-from-scratch");

    // The schema, table, and constraints now exist: insert and read back through the query API.
    let affected = connection
        .to::<Widget>()
        .name("gadget")
        .insert()
        .await
        .expect("insert into published table");
    assert_eq!(affected, 1);

    let rows = connection
        .from::<Widget>()
        .select(|(widget,)| (widget.id, widget.name))
        .collect()
        .await
        .expect("select from published table");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "gadget");
}

#[tokio::test]
#[ignore]
async fn publish_then_introspect_round_trips_schema_model() {
    let mut connection = connect().await;
    let expected = DatabaseModel::from_database::<PublishDemoDb>();

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"publish_demo\" CASCADE")
        .await
        .expect("drop schema");

    squealy_model::publish(&expected, &Postgres, &mut connection)
        .await
        .expect("publish create-from-scratch");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect published schema");
    let actual_schema = actual
        .schemas
        .into_iter()
        .find(|schema| schema.name.as_deref() == Some("publish_demo"))
        .expect("published schema should be introspected");

    assert_eq!(actual_schema, expected.schemas[0]);
}

#[tokio::test]
#[ignore]
async fn publish_then_introspect_preserves_richer_schema_facts() {
    let mut connection = connect().await;
    let expected = rich_model();

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"publish_demo_rich\" CASCADE")
        .await
        .expect("drop schema");

    squealy_model::publish(&expected, &Postgres, &mut connection)
        .await
        .expect("publish rich schema");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect rich schema");
    let actual_schema = actual
        .schemas
        .into_iter()
        .find(|schema| schema.name.as_deref() == Some("publish_demo_rich"))
        .expect("rich schema should be introspected");

    assert_eq!(actual_schema, expected.schemas[0]);
}

fn rich_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("publish_demo_rich".to_owned()),
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
                                mode: IdentityMode::ByDefault,
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
                    ],
                    primary_key: Some(Constraint {
                        name: "pk_memberships".to_owned(),
                        columns: vec!["id".to_owned()],
                    }),
                    foreign_keys: vec![ForeignKeyModel {
                        name: "fk_memberships_tenant_id".to_owned(),
                        columns: vec!["tenant_id".to_owned()],
                        references_schema: Some("publish_demo_rich".to_owned()),
                        references_table: "tenants".to_owned(),
                        references_columns: vec!["id".to_owned()],
                        match_type: Some(ForeignKeyMatch::Full),
                        deferrability: Some(ConstraintDeferrability::InitiallyDeferred),
                        on_delete: Some(ForeignKeyAction::Cascade),
                        on_update: None,
                    }],
                    uniques: Vec::new(),
                    checks: vec![CheckModel {
                        name: "ck_memberships_quota".to_owned(),
                        expression: "(quota > (0)::numeric)".to_owned(),
                    }],
                    indexes: vec![IndexModel {
                        name: "idx_memberships_tenant_id".to_owned(),
                        columns: vec!["tenant_id".to_owned()],
                        expressions: Vec::new(),
                        include_columns: vec!["role_code".to_owned()],
                        unique: false,
                        method: Some(IndexMethod::BTree),
                        directions: vec![IndexDirection::Asc],
                        nulls: vec![IndexNullsOrder::First],
                        collations: Vec::new(),
                        operator_classes: Vec::new(),
                        predicate: Some("(quota > (0)::numeric)".to_owned()),
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
                                mode: IdentityMode::ByDefault,
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
                                expression: "length((slug)::text)".to_owned(),
                                storage: GeneratedStorage::Stored,
                            }),
                        },
                        ColumnModel {
                            name: "settings".to_owned(),
                            comment: None,
                            ty: SqlType::Jsonb,
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
                    indexes: vec![IndexModel {
                        name: "idx_tenants_lower_slug".to_owned(),
                        columns: Vec::new(),
                        expressions: vec!["lower((slug)::text)".to_owned()],
                        include_columns: Vec::new(),
                        unique: false,
                        method: Some(IndexMethod::BTree),
                        directions: vec![IndexDirection::Asc],
                        nulls: Vec::new(),
                        collations: vec![IndexCollation {
                            position: 0,
                            name: "C".to_owned(),
                        }],
                        operator_classes: vec![IndexOperatorClass {
                            position: 0,
                            name: "text_pattern_ops".to_owned(),
                        }],
                        predicate: None,
                    }],
                },
            ],
        }],
    }
}
