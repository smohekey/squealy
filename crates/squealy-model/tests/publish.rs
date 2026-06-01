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
