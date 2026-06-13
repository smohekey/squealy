//! Live end-to-end test of `publish` (create-from-scratch executed against PostgreSQL).
//!
//! `#[ignore]`d like the other PostgreSQL integration tests; run with a database via:
//! `SQUEALY_POSTGRES_URL=... cargo test -p squealy-model --test publish -- --ignored`.

use std::sync::OnceLock;

use squealy::*;
use squealy_postgresql::{Postgres, PostgresConnection};
use tokio::sync::{Mutex, MutexGuard};
use tokio_postgres::NoTls;

/// Serializes the live-database tests in this binary. They all share one database, and
/// `plan_from_database`/`introspect` read the *whole* database, so two tests running concurrently
/// would each see the other's schemas — as spurious drop steps, or by dropping each other's objects.
/// The guard returned by [`connect`] is held for the test's duration.
fn db_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Drops every user schema (including squealy's `__squealy` metadata schema) so each test starts from
/// a clean slate regardless of what earlier tests left behind, then restores the default `public`
/// schema. Run while holding [`db_lock`].
const RESET_DATABASE: &str = "\
DO $$
DECLARE
    schema_name text;
BEGIN
    FOR schema_name IN
        SELECT nspname
        FROM pg_namespace
        WHERE nspname NOT IN ('pg_catalog', 'information_schema')
          AND nspname NOT LIKE 'pg_toast%'
    LOOP
        EXECUTE format('DROP SCHEMA IF EXISTS %I CASCADE', schema_name);
    END LOOP;
END
$$;
CREATE SCHEMA IF NOT EXISTS \"public\"";

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

/// Connects, takes the serialization lock, and resets the database to a clean slate. The returned
/// guard must be held for the test's duration (bind it, e.g. `let (mut connection, _guard) = ...`).
async fn connect() -> (PostgresConnection, MutexGuard<'static, ()>) {
    let guard = db_lock().lock().await;
    let (client, connection) = tokio_postgres::connect(&database_url(), NoTls)
        .await
        .expect("connect to PostgreSQL");
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            panic!("PostgreSQL connection failed: {error}");
        }
    });
    let mut connection = PostgresConnection::new(client);
    connection
        .execute_ddl(RESET_DATABASE)
        .await
        .expect("reset database to a clean slate");
    (connection, guard)
}

#[tokio::test]
#[ignore]
async fn publish_creates_schema_then_round_trips_rows() {
    // `connect` resets to a clean slate, so `render_create`'s plain `CREATE TABLE` is re-runnable.
    let (mut connection, _guard) = connect().await;

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
    let (mut connection, _guard) = connect().await;
    let expected = DatabaseModel::from_database::<PublishDemoDb>();

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
    let (mut connection, _guard) = connect().await;
    let expected = rich_model();

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

#[tokio::test]
#[ignore]
async fn incremental_publish_applies_changed_column_definitions() {
    let (mut connection, _guard) = connect().await;
    let baseline = alter_column_baseline_model();
    let desired = alter_column_desired_model();

    squealy_model::publish(&baseline, &Postgres, &mut connection)
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

    squealy_model::apply_plan(&plan, &Postgres, &mut connection)
        .await
        .expect("apply changed columns");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect altered schema");
    let actual_schema = actual
        .schemas
        .into_iter()
        .find(|schema| schema.name.as_deref() == Some("publish_demo_alter"))
        .expect("altered schema should be introspected");

    assert_eq!(actual_schema, desired.schemas[0]);
}

fn alter_column_baseline_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("publish_demo_alter".to_owned()),
            tables: vec![TableModel {
                name: "events".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "description".to_owned(),
                        comment: Some("Old description".to_owned()),
                        ty: SqlType::String,
                        collation: None,
                        nullable: true,
                        default: Some(DefaultValue::Text("old".to_owned())),
                        identity: None,
                        generated: None,
                    },
                    ColumnModel {
                        name: "status".to_owned(),
                        comment: Some("Event status".to_owned()),
                        ty: SqlType::String,
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
            name: Some("publish_demo_alter".to_owned()),
            tables: vec![TableModel {
                name: "events".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "description".to_owned(),
                        comment: None,
                        ty: SqlType::Varchar(128),
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                    ColumnModel {
                        name: "status".to_owned(),
                        comment: None,
                        ty: SqlType::String,
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
                            collation: None,
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
                        validation: Some(ConstraintValidation::NotValidated),
                        enforcement: None,
                        on_delete: Some(ForeignKeyAction::Cascade),
                        on_update: None,
                    }],
                    uniques: Vec::new(),
                    checks: vec![CheckModel {
                        name: "ck_memberships_quota".to_owned(),
                        expression: "(quota > (0)::numeric)".to_owned(),
                        validation: None,
                        enforcement: None,
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
                            collation: None,
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
                            collation: Some("C".to_owned()),
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
                                expression: "length((slug)::text)".to_owned(),
                                storage: GeneratedStorage::Stored,
                            }),
                        },
                        ColumnModel {
                            name: "settings".to_owned(),
                            comment: None,
                            ty: SqlType::Jsonb,
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
