use std::future::poll_fn;

use futures_core::Stream;
use squealy::*;
use squealy_postgresql::{Postgres, PostgresConnection, PostgresError};
use tokio_postgres::NoTls;

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct TransactionUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationDefaulted<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(default = value("anonymous"))]
    name: C::Type<'scope, String>,
    #[column(default = value(42))]
    score: C::Type<'scope, i32>,
    #[column(default = value(true))]
    active: C::Type<'scope, bool>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationNullable<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(nullable)]
    note: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct JoinUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct JoinPost<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(references(JoinUser::id))]
    user_id: C::Type<'scope, i32>,
    title: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationTypes<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    small: C::Type<'scope, i16>,
    medium: C::Type<'scope, i32>,
    large: C::Type<'scope, i64>,
    signed_wide: C::Type<'scope, i128>,
    unsigned_large: C::Type<'scope, u64>,
    unsigned_wide: C::Type<'scope, u128>,
    single: C::Type<'scope, f32>,
    double: C::Type<'scope, f64>,
    flag: C::Type<'scope, bool>,
    #[column(db_type = "varchar(64)")]
    label: C::Type<'scope, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
pub struct IntegrationRecordId(i32);

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationNewtype<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, IntegrationRecordId>,
    #[column(nullable)]
    parent_id: C::Type<'scope, IntegrationRecordId>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct MissingTable<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
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

    let ddl_backend = Postgres;
    create_table::<IntegrationUser>(&client, &ddl_backend).await;
    create_table::<IntegrationDefaulted>(&client, &ddl_backend).await;
    create_table::<IntegrationNullable>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let (ada, ada_count) = connection
        .to::<IntegrationUser>()
        .name("Ada")
        .insert_returning(|user| user)
        .fetch_one_with_affected()
        .await
        .expect("insert Ada");
    assert_eq!(ada_count, 1);
    assert_eq!(ada.name, "Ada");

    let affected = connection
        .to::<IntegrationUser>()
        .name("Grace")
        .insert()
        .await
        .expect("insert Grace");
    assert_eq!(affected, 1);

    let insert_preparable = connection
        .to::<IntegrationUser>()
        .name("Prepared")
        .insert_returning(|user| user.name);
    let prepared_insert = insert_preparable.prepare().await.expect("prepare insert");
    assert_eq!(
        prepared_insert
            .fetch_one(())
            .await
            .expect("execute prepared insert"),
        "Prepared"
    );
    assert_eq!(
        prepared_insert
            .fetch_one(())
            .await
            .expect("execute prepared insert again"),
        "Prepared"
    );

    let dynamic_insert_preparable = connection
        .to::<IntegrationUser>()
        .name(param::<IntegrationUserName>())
        .insert_returning(|user| user.name);
    let prepared_dynamic_insert = dynamic_insert_preparable
        .prepare()
        .await
        .expect("prepare dynamic insert");
    assert_eq!(
        prepared_dynamic_insert
            .fetch_one(("Runtime Inserted".to_owned(),))
            .await
            .expect("execute dynamic prepared insert"),
        "Runtime Inserted"
    );

    let update_preparable = connection
        .to::<IntegrationUser>()
        .name("Prepared Updated")
        .where_(|user| user.name.equals("Prepared"))
        .update_returning(|user| user.name);
    let prepared_update = update_preparable.prepare().await.expect("prepare update");
    let (updated_names, updated_count) = prepared_update
        .collect_with_affected(())
        .await
        .expect("execute prepared update");
    assert_eq!(updated_count, 2);
    assert_eq!(
        updated_names,
        vec!["Prepared Updated".to_owned(), "Prepared Updated".to_owned()]
    );

    let select_preparable = connection
        .from::<IntegrationUser>()
        .where_(|user| user.name.equals("Prepared Updated"))
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user.name);
    let prepared_select = select_preparable.prepare().await.expect("prepare select");
    assert_eq!(
        prepared_select
            .collect(())
            .await
            .expect("execute prepared select"),
        vec!["Prepared Updated".to_owned(), "Prepared Updated".to_owned()]
    );

    let dynamic_select_preparable = connection
        .from::<IntegrationUser>()
        .where_(|user| user.name.equals(param::<IntegrationUserName>()))
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user.name);
    let prepared_dynamic_select = dynamic_select_preparable
        .prepare()
        .await
        .expect("prepare dynamic select");
    assert_eq!(
        prepared_dynamic_select
            .collect(("Prepared Updated",))
            .await
            .expect("execute dynamic prepared select"),
        vec!["Prepared Updated".to_owned(), "Prepared Updated".to_owned()]
    );

    let dynamic_update_preparable = connection
        .to::<IntegrationUser>()
        .name(param::<IntegrationUserName>())
        .where_(|user| user.name.equals(param::<IntegrationUserName>()))
        .update_returning(|user| user.name);
    let prepared_dynamic_update = dynamic_update_preparable
        .prepare()
        .await
        .expect("prepare dynamic update");
    let (runtime_updated_names, runtime_updated_count) = prepared_dynamic_update
        .collect_with_affected(("Runtime Prepared Updated".to_owned(), "Prepared Updated"))
        .await
        .expect("execute dynamic prepared update");
    assert_eq!(runtime_updated_count, 2);
    assert_eq!(
        runtime_updated_names,
        vec![
            "Runtime Prepared Updated".to_owned(),
            "Runtime Prepared Updated".to_owned()
        ]
    );

    let dynamic_delete_preparable = connection
        .from::<IntegrationUser>()
        .where_(|user| user.name.equals(param::<IntegrationUserName>()))
        .delete_returning(|user| user.id);
    let prepared_dynamic_delete = dynamic_delete_preparable
        .prepare()
        .await
        .expect("prepare dynamic delete");
    let (deleted_ids, deleted_count) = prepared_dynamic_delete
        .collect_with_affected(("Runtime Prepared Updated",))
        .await
        .expect("execute dynamic prepared delete");
    assert_eq!(deleted_count, 2);
    assert_eq!(deleted_ids.len(), 2);
    let (deleted_runtime_ids, deleted_runtime_count) = prepared_dynamic_delete
        .collect_with_affected(("Runtime Inserted",))
        .await
        .expect("execute dynamic prepared delete again");
    assert_eq!(deleted_runtime_count, 1);
    assert_eq!(deleted_runtime_ids.len(), 1);

    let users = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user)
        .collect()
        .await
        .expect("fetch inserted users");

    assert_eq!(users.len(), 2);
    assert_eq!(users[0].name, "Ada");
    assert_eq!(users[1].name, "Grace");

    let (updated_rows, updated_count) = connection
        .to::<IntegrationUser>()
        .name("Ada Lovelace")
        .where_(|user| user.id.equals(ada.id))
        .update_returning(|user| user)
        .collect_with_affected()
        .await
        .expect("update Ada");
    assert_eq!(updated_count, 1);
    assert_eq!(updated_rows.len(), 1);
    let updated_ada = updated_rows.into_iter().next().unwrap();
    assert_eq!(updated_ada.id, ada.id);
    assert_eq!(updated_ada.name, "Ada Lovelace");

    let (deleted_rows, deleted_count) = connection
        .from::<IntegrationUser>()
        .where_(|user| user.name.equals("Grace"))
        .delete_returning(|user| user)
        .collect_with_affected()
        .await
        .expect("delete Grace");
    assert_eq!(deleted_count, 1);
    assert_eq!(deleted_rows.len(), 1);
    let deleted_grace = deleted_rows.into_iter().next().unwrap();
    assert_eq!(deleted_grace.name, "Grace");

    let remaining = connection
        .from::<IntegrationUser>()
        .select(|(user,)| user)
        .collect()
        .await
        .expect("fetch remaining users");

    assert_eq!(remaining, vec![updated_ada]);

    let stream_query = connection
        .from::<IntegrationUser>()
        .select(|(user,)| user.name);
    let mut rows = Box::pin(stream_query.fetch());
    let first = poll_fn(|cx| rows.as_mut().poll_next(cx))
        .await
        .expect("stream should yield one row")
        .expect("stream row should decode");
    let second = poll_fn(|cx| rows.as_mut().poll_next(cx)).await;

    assert_eq!(first, "Ada Lovelace");
    assert!(second.is_none());

    let defaulted = connection
        .to::<IntegrationDefaulted>()
        .insert_returning(|record| record)
        .fetch_one()
        .await
        .expect("insert defaulted record");

    assert_eq!(defaulted.name, "anonymous");
    assert_eq!(defaulted.score, 42);
    assert!(defaulted.active);

    let nullable_id = connection
        .to::<IntegrationNullable>()
        .note(None::<String>)
        .insert_returning(|record| record.id)
        .fetch_one()
        .await
        .expect("insert nullable record");

    let affected = connection
        .to::<IntegrationNullable>()
        .note(None::<String>)
        .where_(|record| record.id.equals(nullable_id))
        .update()
        .await
        .expect("update nullable record");

    assert_eq!(affected, 1);

    let nullable_row = connection
        .from::<IntegrationNullable>()
        .where_(|record| record.id.equals(nullable_id))
        .select(|(record,)| record)
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

    let ddl_backend = Postgres;
    create_table::<JoinUser>(&client, &ddl_backend).await;
    create_table::<JoinPost>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let ada = connection
        .to::<JoinUser>()
        .name("Ada")
        .insert_returning(|user| user)
        .fetch_one()
        .await
        .expect("insert Ada");

    connection
        .to::<JoinUser>()
        .name("Grace")
        .insert()
        .await
        .expect("insert Grace");

    connection
        .to::<JoinPost>()
        .user_id(ada.id)
        .title("Notes on the Analytical Engine")
        .insert()
        .await
        .expect("insert post");

    let rows = connection
        .from::<JoinUser>()
        .join::<JoinPost>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .order_by(|(_user, post)| post.id.asc())
        .select(|(user, post)| (user, post))
        .collect()
        .await
        .expect("fetch joined rows");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0.name, "Ada");
    assert_eq!(rows[0].1.user_id, ada.id);
    assert_eq!(rows[0].1.title, "Notes on the Analytical Engine");

    let source_first_rows = connection
        .from::<JoinUser>()
        .join::<JoinPost>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .order_by(|(_user, post)| post.id.asc())
        .select(|(user, post)| (user.name, post.title))
        .collect()
        .await
        .expect("fetch joined rows through source-first query");

    assert_eq!(
        source_first_rows,
        vec![(
            "Ada".to_owned(),
            "Notes on the Analytical Engine".to_owned()
        )]
    );

    let source_first_left_join_rows = connection
        .from::<JoinUser>()
        .left_join::<JoinPost>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .order_by(|(user, _post)| user.id.asc())
        .select(|(user, post)| (user.name, post.title))
        .collect()
        .await
        .expect("fetch left joined rows through source-first query");

    assert_eq!(
        source_first_left_join_rows,
        vec![
            (
                "Ada".to_owned(),
                Some("Notes on the Analytical Engine".to_owned())
            ),
            ("Grace".to_owned(), None),
        ]
    );
}

#[tokio::test]
#[ignore]
async fn postgres_round_trips_primitive_types() {
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_typess")
        .await
        .expect("drop old types table");

    let ddl_backend = Postgres;
    create_table::<IntegrationTypes>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let stored = connection
        .to::<IntegrationTypes>()
        .small(7i16)
        .medium(1_000i32)
        .large(9_000_000_000i64)
        .signed_wide(i128::MIN)
        .unsigned_large(u64::MAX)
        .unsigned_wide(u128::MAX)
        .single(1.5f32)
        .double(2.5f64)
        .flag(true)
        .label("mixed")
        .insert_returning(|record| record)
        .fetch_one()
        .await
        .expect("insert typed record");

    assert_eq!(stored.small, 7);
    assert_eq!(stored.medium, 1_000);
    assert_eq!(stored.large, 9_000_000_000);
    assert_eq!(stored.signed_wide, i128::MIN);
    assert_eq!(stored.unsigned_large, u64::MAX);
    assert_eq!(stored.unsigned_wide, u128::MAX);
    assert_eq!(stored.single, 1.5);
    assert_eq!(stored.double, 2.5);
    assert!(stored.flag);
    assert_eq!(stored.label, "mixed");
}

#[tokio::test]
#[ignore]
async fn postgres_round_trips_transparent_newtypes() {
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_newtypes")
        .await
        .expect("drop old newtype table");

    let ddl_backend = Postgres;
    create_table::<IntegrationNewtype>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let inserted = connection
        .to::<IntegrationNewtype>()
        .id(IntegrationRecordId(7))
        .parent_id(Some(IntegrationRecordId(3)))
        .name("wrapped")
        .insert_returning(|record| record)
        .fetch_one()
        .await
        .expect("insert newtype record");

    assert_eq!(inserted.id, IntegrationRecordId(7));
    assert_eq!(inserted.parent_id, Some(IntegrationRecordId(3)));
    assert_eq!(inserted.name, "wrapped");

    let ids = connection
        .from::<IntegrationNewtype>()
        .where_(|record| record.id.equals(IntegrationRecordId(7)))
        .select(|(record,)| record.id)
        .collect()
        .await
        .expect("select newtype ids");

    assert_eq!(ids, vec![IntegrationRecordId(7)]);
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
        .from::<MissingTable>()
        .select(|(row,)| row)
        .collect()
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

    let ddl_backend = Postgres;
    create_table::<TransactionUser>(&client, &ddl_backend).await;

    let mut connection = PostgresConnection::new(client);

    let committed_name = connection
        .transaction(async |transaction| {
            let user = transaction
                .to::<TransactionUser>()
                .name("Committed")
                .insert_returning(|user| user)
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
                .to::<TransactionUser>()
                .name("Rolled back")
                .insert()
                .await?;
            Err(PostgresError::NoRows)
        })
        .await;

    assert!(matches!(rolled_back, Err(PostgresError::NoRows)));

    let users = connection
        .from::<TransactionUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user.name)
        .collect()
        .await
        .expect("fetch committed users");

    assert_eq!(users, vec!["Committed".to_owned()]);
}

async fn create_table<S>(client: &tokio_postgres::Client, ddl_backend: &Postgres)
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
