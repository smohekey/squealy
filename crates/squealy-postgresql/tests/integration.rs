use squealy::*;
use squealy_postgresql::PostgresConnection;
use tokio_postgres::NoTls;

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
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
        .batch_execute("DROP TABLE IF EXISTS integration_users")
        .await
        .expect("drop old integration table");

    let ddl_backend = PostgresConnection::default();
    let mut ddl = Vec::new();
    let table = <IntegrationUser<'static, ColumnExpr> as SchemaTable>::column_names();
    ddl_backend
        .write_table(&table, &mut ddl)
        .expect("render integration table DDL");
    let ddl = String::from_utf8(ddl).expect("DDL should be valid UTF-8");
    client.batch_execute(&ddl).await.expect("create table");

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
}
