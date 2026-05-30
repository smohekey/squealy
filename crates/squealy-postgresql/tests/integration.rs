use squealy::*;
use squealy_postgresql::PostgresConnection;
use tokio_postgres::NoTls;

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationDefaulted<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    #[column(default = value("anonymous"), db_type = "text")]
    name: C::Type<'scope, String>,
    #[column(default = value(42), db_type = "integer")]
    score: C::Type<'scope, i32>,
    #[column(default = value(true), db_type = "boolean")]
    active: C::Type<'scope, bool>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationNullable<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    #[column(nullable, db_type = "text")]
    note: C::Type<'scope, String>,
}

fn database_url() -> String {
    std::env::var("SQUEALY_POSTGRES_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:55432/squealy_test".to_owned())
}

async fn connect() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(&database_url(), NoTls)
        .await
        .expect("connect to PostgreSQL");

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            panic!("PostgreSQL connection failed: {error}");
        }
    });

    client
}

#[tokio::test]
#[ignore]
async fn postgres_executes_insert_returning_and_selects_rows() {
    let client = connect().await;
    client
        .batch_execute(
            "DROP TABLE IF EXISTS integration_users; DROP TABLE IF EXISTS integration_defaulteds; DROP TABLE IF EXISTS integration_nullables",
        )
        .await
        .expect("drop old integration tables");

    let ddl_backend = PostgresConnection::default();
    create_table::<IntegrationUser>(&client, &ddl_backend).await;
    create_table::<IntegrationDefaulted>(&client, &ddl_backend).await;
    create_table::<IntegrationNullable>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let ada = connection
        .insert::<IntegrationUser>()
        .name("Ada")
        .returning(|user| user)
        .fetch_one()
        .await
        .expect("insert Ada");
    assert_eq!(ada.name, "Ada");

    let affected = connection
        .insert::<IntegrationUser>()
        .name("Grace")
        .execute()
        .await
        .expect("insert Grace");
    assert_eq!(affected, 1);

    let users = connection
        .select(|q| {
            let user = q.from::<IntegrationUser>();
            q.order_by(user.id.asc());
            q.returning(user)
        })
        .fetch_all()
        .await
        .expect("fetch inserted users");

    assert_eq!(users.len(), 2);
    assert_eq!(users[0].name, "Ada");
    assert_eq!(users[1].name, "Grace");

    let updated_ada = connection
        .update::<IntegrationUser>()
        .name("Ada Lovelace")
        .where_(|user| user.id.equals(ada.id))
        .returning(|user| user)
        .fetch_one()
        .await
        .expect("update Ada");
    assert_eq!(updated_ada.id, ada.id);
    assert_eq!(updated_ada.name, "Ada Lovelace");

    let deleted_grace = connection
        .delete::<IntegrationUser>()
        .where_(|user| user.name.equals("Grace"))
        .returning(|user| user)
        .fetch_one()
        .await
        .expect("delete Grace");
    assert_eq!(deleted_grace.name, "Grace");

    let remaining = connection
        .select(|q| {
            let user = q.from::<IntegrationUser>();
            q.returning(user)
        })
        .fetch_all()
        .await
        .expect("fetch remaining users");

    assert_eq!(remaining, vec![updated_ada]);

    let defaulted = connection
        .insert::<IntegrationDefaulted>()
        .returning(|record| record)
        .fetch_one()
        .await
        .expect("insert defaulted record");

    assert_eq!(defaulted.name, "anonymous");
    assert_eq!(defaulted.score, 42);
    assert!(defaulted.active);

    let nullable_id = connection
        .insert::<IntegrationNullable>()
        .note(None::<String>)
        .returning(|record| record.id)
        .fetch_one()
        .await
        .expect("insert nullable record");

    let affected = connection
        .update::<IntegrationNullable>()
        .note(None::<String>)
        .where_(|record| record.id.equals(nullable_id))
        .execute()
        .await
        .expect("update nullable record");

    assert_eq!(affected, 1);
}

async fn create_table<S>(client: &tokio_postgres::Client, ddl_backend: &PostgresConnection)
where
    S: SchemaTable,
    S::WithColumn<'static, ColumnName>: Table + Sync,
{
    let mut ddl = Vec::new();
    let table = S::column_names();
    ddl_backend
        .write_table(&table, &mut ddl)
        .expect("render integration table DDL");
    let ddl = String::from_utf8(ddl).expect("DDL should be valid UTF-8");
    client.batch_execute(&ddl).await.expect("create table");
}
