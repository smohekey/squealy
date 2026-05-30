use std::future::poll_fn;

use futures_core::Stream;
use futures_util::TryStreamExt;
use squealy::*;
use squealy_postgresql::{PostgresConnection, PostgresError};
use tokio_postgres::NoTls;

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct TransactionUser<'scope, C: ColumnMode = ColumnExpr> {
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

#[derive(Clone, Debug, PartialEq, Table)]
struct JoinUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    #[column(db_type = "text")]
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct JoinPost<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    #[column(references(JoinUser::id), db_type = "integer")]
    user_id: C::Type<'scope, i32>,
    #[column(db_type = "text")]
    title: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationTypes<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    #[column(db_type = "smallint")]
    small: C::Type<'scope, i16>,
    #[column(db_type = "integer")]
    medium: C::Type<'scope, i32>,
    #[column(db_type = "bigint")]
    large: C::Type<'scope, i64>,
    #[column(db_type = "real")]
    single: C::Type<'scope, f32>,
    #[column(db_type = "double precision")]
    double: C::Type<'scope, f64>,
    #[column(db_type = "boolean")]
    flag: C::Type<'scope, bool>,
    #[column(db_type = "text")]
    label: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct MissingTable<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
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

    let (ada, ada_count) = connection
        .insert::<IntegrationUser>()
        .name("Ada")
        .returning(|user| user)
        .fetch_one_with_affected()
        .await
        .expect("insert Ada");
    assert_eq!(ada_count, 1);
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
        .fetch()
        .try_collect::<Vec<_>>()
        .await
        .expect("fetch inserted users");

    assert_eq!(users.len(), 2);
    assert_eq!(users[0].name, "Ada");
    assert_eq!(users[1].name, "Grace");

    let (updated_rows, updated_count) = connection
        .update::<IntegrationUser>()
        .name("Ada Lovelace")
        .where_(|user| user.id.equals(ada.id))
        .returning(|user| user)
        .fetch_all_with_affected()
        .await
        .expect("update Ada");
    assert_eq!(updated_count, 1);
    assert_eq!(updated_rows.len(), 1);
    let updated_ada = updated_rows.into_iter().next().unwrap();
    assert_eq!(updated_ada.id, ada.id);
    assert_eq!(updated_ada.name, "Ada Lovelace");

    let (deleted_rows, deleted_count) = connection
        .delete::<IntegrationUser>()
        .where_(|user| user.name.equals("Grace"))
        .returning(|user| user)
        .fetch_all_with_affected()
        .await
        .expect("delete Grace");
    assert_eq!(deleted_count, 1);
    assert_eq!(deleted_rows.len(), 1);
    let deleted_grace = deleted_rows.into_iter().next().unwrap();
    assert_eq!(deleted_grace.name, "Grace");

    let remaining = connection
        .select(|q| {
            let user = q.from::<IntegrationUser>();
            q.returning(user)
        })
        .fetch()
        .try_collect::<Vec<_>>()
        .await
        .expect("fetch remaining users");

    assert_eq!(remaining, vec![updated_ada]);

    let stream_query = connection.select(|q| {
        let user = q.from::<IntegrationUser>();
        q.returning(user.name)
    });
    let mut rows = Box::pin(stream_query.fetch());
    let first = poll_fn(|cx| rows.as_mut().poll_next(cx))
        .await
        .expect("stream should yield one row")
        .expect("stream row should decode");
    let second = poll_fn(|cx| rows.as_mut().poll_next(cx)).await;

    assert_eq!(first, "Ada Lovelace");
    assert!(second.is_none());

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

    let nullable_row = connection
        .select(|q| {
            let record = q.from::<IntegrationNullable>();
            q.where_(record.id.equals(nullable_id));
            q.returning(record)
        })
        .fetch_one()
        .await
        .expect("fetch nullable record");

    assert_eq!(nullable_row.id, nullable_id);
    assert_eq!(nullable_row.note, None);
}

#[tokio::test]
#[ignore]
async fn postgres_inner_joins_across_tables() {
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS join_posts; DROP TABLE IF EXISTS join_users")
        .await
        .expect("drop old join tables");

    let ddl_backend = PostgresConnection::default();
    create_table::<JoinUser>(&client, &ddl_backend).await;
    create_table::<JoinPost>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let ada = connection
        .insert::<JoinUser>()
        .name("Ada")
        .returning(|user| user)
        .fetch_one()
        .await
        .expect("insert Ada");

    connection
        .insert::<JoinPost>()
        .user_id(ada.id)
        .title("Notes on the Analytical Engine")
        .execute()
        .await
        .expect("insert post");

    let rows = connection
        .select(|q| {
            let user = q.from::<JoinUser>();
            let post = q.join::<JoinPost>(|post| post.user_id.equals(user.id));
            q.order_by(post.id.asc());
            q.returning((user, post))
        })
        .fetch()
        .try_collect::<Vec<_>>()
        .await
        .expect("fetch joined rows");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0.name, "Ada");
    assert_eq!(rows[0].1.user_id, ada.id);
    assert_eq!(rows[0].1.title, "Notes on the Analytical Engine");
}

#[tokio::test]
#[ignore]
async fn postgres_round_trips_primitive_types() {
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_typess")
        .await
        .expect("drop old types table");

    let ddl_backend = PostgresConnection::default();
    create_table::<IntegrationTypes>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let stored = connection
        .insert::<IntegrationTypes>()
        .small(7i16)
        .medium(1_000i32)
        .large(9_000_000_000i64)
        .single(1.5f32)
        .double(2.5f64)
        .flag(true)
        .label("mixed")
        .returning(|record| record)
        .fetch_one()
        .await
        .expect("insert typed record");

    assert_eq!(stored.small, 7);
    assert_eq!(stored.medium, 1_000);
    assert_eq!(stored.large, 9_000_000_000);
    assert_eq!(stored.single, 1.5);
    assert_eq!(stored.double, 2.5);
    assert!(stored.flag);
    assert_eq!(stored.label, "mixed");
}

#[tokio::test]
#[ignore]
async fn postgres_surfaces_database_errors() {
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS missing_tables")
        .await
        .expect("ensure table is absent");

    let connection = PostgresConnection::new(client);

    let result = connection
        .select(|q| {
            let row = q.from::<MissingTable>();
            q.returning(row)
        })
        .fetch()
        .try_collect::<Vec<_>>()
        .await;

    assert!(
        matches!(result, Err(PostgresError::Database(_))),
        "expected a database error, got {result:?}"
    );
}

#[tokio::test]
#[ignore]
async fn postgres_runs_transaction_closures() {
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS transaction_users")
        .await
        .expect("drop old transaction table");

    let ddl_backend = PostgresConnection::default();
    create_table::<TransactionUser>(&client, &ddl_backend).await;

    let mut connection = PostgresConnection::new(client);

    let committed_name = connection
        .transaction(async |transaction| {
            let user = transaction
                .insert::<TransactionUser>()
                .name("Committed")
                .returning(|user| user)
                .fetch_one()
                .await?;
            Ok(user.name)
        })
        .await
        .expect("commit transaction");

    assert_eq!(committed_name, "Committed");

    let rolled_back: Result<(), PostgresError> = connection
        .transaction(async |transaction| {
            transaction
                .insert::<TransactionUser>()
                .name("Rolled back")
                .execute()
                .await?;
            Err(PostgresError::NoRows)
        })
        .await;

    assert!(matches!(rolled_back, Err(PostgresError::NoRows)));

    let users = connection
        .select(|q| {
            let user = q.from::<TransactionUser>();
            q.order_by(user.id.asc());
            q.returning(user.name)
        })
        .fetch()
        .try_collect::<Vec<_>>()
        .await
        .expect("fetch committed users");

    assert_eq!(users, vec!["Committed".to_owned()]);
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
