use std::future::poll_fn;
use std::sync::OnceLock;

use futures_core::Stream;
use squealy::*;
use squealy_postgresql::{Postgres, PostgresConnection, PostgresError};
use tokio::sync::Mutex;
use tokio_postgres::NoTls;

/// Serializes the live-database tests in this binary. They share one Postgres database and create or
/// drop fixture tables in `public`, so two running concurrently would clobber each other. Each test
/// holds this guard for its duration.
fn db_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

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
    note: C::Type<'scope, Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationBytes<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    data: C::Type<'scope, Vec<u8>>,
    maybe: C::Type<'scope, Option<Vec<u8>>>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationFixedBytes<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    key: C::Type<'scope, [u8; 4]>,
    nonce: C::Type<'scope, Option<[u8; 2]>>,
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
    parent_id: C::Type<'scope, Option<IntegrationRecordId>>,
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
    let _db_guard = db_lock().lock().await;
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

    let explicitly_defaulted = connection
        .to_columns::<IntegrationDefaulted, (IntegrationDefaultedName, IntegrationDefaultedScore)>()
        .row((default(), default()))
        .insert_returning(|record| record)
        .fetch_one()
        .await
        .expect("insert explicitly defaulted record");

    assert_eq!(explicitly_defaulted.name, "anonymous");
    assert_eq!(explicitly_defaulted.score, 42);
    assert!(explicitly_defaulted.active);

    let incremented = connection
        .to_columns::<IntegrationDefaulted, (IntegrationDefaultedScore,)>()
        .set(|record| (record.score + 1,))
        .where_(|record| record.id.equals(explicitly_defaulted.id))
        .update_returning(|record| record.score)
        .fetch_one()
        .await
        .expect("increment defaulted record score");

    assert_eq!(incremented, 43);

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
    let _db_guard = db_lock().lock().await;
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
async fn postgres_cross_and_self_joins() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS join_posts; DROP TABLE IF EXISTS join_users")
        .await
        .expect("drop old join tables");

    let ddl_backend = Postgres;
    create_table::<JoinUser>(&client, &ddl_backend).await;
    create_table::<JoinPost>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    // 2 users, 3 posts.
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
    for title in ["p1", "p2", "p3"] {
        connection
            .to::<JoinPost>()
            .user_id(ada.id)
            .title(title)
            .insert()
            .await
            .expect("insert post");
    }

    // CROSS JOIN: the Cartesian product has |users| * |posts| = 2 * 3 = 6 rows.
    let crossed = connection
        .from::<JoinUser>()
        .cross_join::<JoinPost>()
        .select(|(user, post)| (user.id, post.id))
        .collect()
        .await
        .expect("fetch cross-joined rows");
    assert_eq!(crossed.len(), 6);

    // Self-join: the same table aliased twice. Joining posts to themselves on `id` yields one row per
    // post, each pairing a post with itself (verifying the two aliases resolve to the same row).
    let self_pairs = connection
        .from::<JoinPost>()
        .join::<JoinPost>()
        .on(|(post,), other| post.id.equals(other.id))
        .order_by(|(post, _other)| post.id.asc())
        .select(|(post, other)| (post.title, other.title))
        .collect()
        .await
        .expect("fetch self-joined rows");
    assert_eq!(
        self_pairs,
        vec![
            ("p1".to_owned(), "p1".to_owned()),
            ("p2".to_owned(), "p2".to_owned()),
            ("p3".to_owned(), "p3".to_owned()),
        ]
    );
}

#[tokio::test]
#[ignore]
async fn postgres_right_and_full_join_nullability() {
    let _db_guard = db_lock().lock().await;
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
        .title("Notes")
        .insert()
        .await
        .expect("insert post");

    // RIGHT JOIN from posts to users: every user appears; Grace (no post) yields a NULL post title, so
    // the base (post) side is nullable while the joined (user) side stays non-null.
    let right: Vec<(String, Option<String>)> = connection
        .from::<JoinPost>()
        .right_join::<JoinUser>()
        .on(|(post,), user| post.user_id.equals(user.id))
        .order_by(|(_post, user)| user.name.asc())
        .select(|(post, user)| (user.name, post.title))
        .collect()
        .await
        .expect("right join");
    assert_eq!(
        right,
        vec![
            ("Ada".to_owned(), Some("Notes".to_owned())),
            ("Grace".to_owned(), None),
        ]
    );

    // FULL JOIN makes BOTH sides nullable, so the user name is now `Option<String>` too.
    let full: Vec<(Option<String>, Option<String>)> = connection
        .from::<JoinPost>()
        .full_join::<JoinUser>()
        .on(|(post,), user| post.user_id.equals(user.id))
        .order_by(|(_post, user)| user.name.asc())
        .select(|(post, user)| (user.name, post.title))
        .collect()
        .await
        .expect("full join");
    assert_eq!(
        full,
        vec![
            (Some("Ada".to_owned()), Some("Notes".to_owned())),
            (Some("Grace".to_owned()), None),
        ]
    );
}

#[tokio::test]
#[ignore]
async fn postgres_correlated_exists_and_in_subqueries() {
    let _db_guard = db_lock().lock().await;
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

    // Correlated EXISTS: only users that have at least one post.
    let authors = connection
        .from::<JoinUser>()
        .where_correlated(|(user,), sub| {
            exists(
                sub.from::<JoinPost>()
                    .where_(|post| post.user_id.equals(user.id))
                    .select_subquery(|(post,)| post.user_id),
            )
        })
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user.name)
        .collect()
        .await
        .expect("fetch authors via correlated EXISTS");
    assert_eq!(authors, vec!["Ada".to_owned()]);

    // IN (subquery): users whose id appears among post authors.
    let ids = connection
        .from::<JoinUser>()
        .where_correlated(|(user,), sub| {
            user.id.in_subquery(
                sub.from::<JoinPost>()
                    .select_subquery(|(post,)| post.user_id),
            )
        })
        .select(|(user,)| user.name)
        .collect()
        .await
        .expect("fetch authors via IN (subquery)");
    assert_eq!(ids, vec!["Ada".to_owned()]);

    // Scalar subquery in a projection: count each user's posts, decoded end to end. A scalar
    // subquery is always nullable (zero matching rows is SQL NULL), so it decodes as `Option`.
    let counts = connection
        .from::<JoinUser>()
        .order_by(|(user,)| user.id.asc())
        .select_correlated(|(user,), sub| {
            scalar_subquery(
                sub.from::<JoinPost>()
                    .where_(|post| post.user_id.equals(user.id))
                    .select_subquery(|(post,)| post.id.count()),
            )
        })
        .collect()
        .await
        .expect("fetch post counts via scalar subquery");
    assert_eq!(counts, vec![Some(1_i64), Some(0_i64)]);
}

#[tokio::test]
#[ignore]
async fn postgres_inserts_multiple_rows() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_users")
        .await
        .expect("drop old integration_users table");

    let ddl_backend = Postgres;
    create_table::<IntegrationUser>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);
    let (names, affected) = connection
        .to_columns::<IntegrationUser, (IntegrationUserName,)>()
        .row(("Ada",))
        .row(("Grace",))
        .insert_returning(|user| user.name)
        .collect_with_affected()
        .await
        .expect("insert multiple users");

    assert_eq!(affected, 2);
    assert_eq!(names, vec!["Ada".to_owned(), "Grace".to_owned()]);

    let dynamic_multi_insert_preparable = connection
        .to_columns::<IntegrationUser, (IntegrationUserName,)>()
        .row((param::<IntegrationUserName>(),))
        .row((param::<IntegrationUserName>(),))
        .insert_returning(|user| user.name);
    let prepared_dynamic_multi_insert = dynamic_multi_insert_preparable
        .prepare()
        .await
        .expect("prepare dynamic multi insert");
    let (runtime_names, runtime_count) = prepared_dynamic_multi_insert
        .collect_with_affected(("Runtime One".to_owned(), "Runtime Two".to_owned()))
        .await
        .expect("execute dynamic multi insert");
    assert_eq!(runtime_count, 2);
    assert_eq!(
        runtime_names,
        vec!["Runtime One".to_owned(), "Runtime Two".to_owned()]
    );
}

#[tokio::test]
#[ignore]
async fn postgres_round_trips_primitive_types() {
    let _db_guard = db_lock().lock().await;
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

    let quotients = connection
        .from::<IntegrationTypes>()
        .select(|(record,)| {
            (
                record.signed_wide / 2i128,
                record.unsigned_large / 2u64,
                record.unsigned_wide / 2u128,
            )
        })
        .fetch_one()
        .await
        .expect("select numeric quotients");

    assert_eq!(quotients.0, (i128::MIN as f64) / 2.0);
    assert_eq!(quotients.1, (u64::MAX as f64) / 2.0);
    assert_eq!(quotients.2, (u128::MAX as f64) / 2.0);
}

#[tokio::test]
#[ignore]
async fn postgres_round_trips_transparent_newtypes() {
    let _db_guard = db_lock().lock().await;
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
    let _db_guard = db_lock().lock().await;
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
    let _db_guard = db_lock().lock().await;
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

#[cfg(feature = "uuid")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
#[column_type(db_type = "uuid")]
struct IntegrationUuid(uuid::Uuid);

#[cfg(feature = "uuid")]
#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationUuidRecord<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, IntegrationUuid>,
    name: C::Type<'scope, String>,
}

#[cfg(feature = "uuid")]
#[tokio::test]
#[ignore]
async fn postgres_round_trips_native_uuid() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_uuid_records")
        .await
        .expect("drop old uuid table");

    let ddl_backend = Postgres;
    create_table::<IntegrationUuidRecord>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);
    let id = IntegrationUuid(uuid::Uuid::from_u128(
        0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
    ));

    let inserted = connection
        .to::<IntegrationUuidRecord>()
        .id(id)
        .name("with-uuid")
        .insert_returning(|record| record)
        .fetch_one()
        .await
        .expect("insert uuid record");

    assert_eq!(inserted.id, id);
    assert_eq!(inserted.name, "with-uuid");

    // The value round-trips through a real `uuid` column and filters correctly.
    let ids = connection
        .from::<IntegrationUuidRecord>()
        .where_(|record| record.id.equals(id))
        .select(|(record,)| record.id)
        .collect()
        .await
        .expect("select uuid ids");

    assert_eq!(ids, vec![id]);

    // The custom `uuid` type also works as a prepared *runtime* parameter, not just an inline
    // literal — supplying an `IntegrationUuid` to `param::<IntegrationUuid>()` encodes natively.
    let uuid_param_select = connection
        .from::<IntegrationUuidRecord>()
        .where_(|record| record.id.equals(param::<IntegrationUuid>()))
        .select(|(record,)| record.name);
    let prepared = uuid_param_select
        .prepare()
        .await
        .expect("prepare uuid-param select");
    let names = prepared
        .collect((id,))
        .await
        .expect("execute prepared uuid-param select");

    assert_eq!(names, vec!["with-uuid".to_owned()]);
}

#[cfg(feature = "bytes")]
#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationBytesCrate<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    payload: C::Type<'scope, bytes::Bytes>,
    maybe: C::Type<'scope, Option<bytes::Bytes>>,
}

#[cfg(feature = "bytes")]
#[tokio::test]
#[ignore]
async fn postgres_round_trips_bytes_crate_column() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_bytes_crates")
        .await
        .expect("drop old bytes-crate table");

    let ddl_backend = Postgres;
    create_table::<IntegrationBytesCrate>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);
    let payload = bytes::Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF]);

    let inserted = connection
        .to::<IntegrationBytesCrate>()
        .payload(payload.clone())
        .maybe(Some(bytes::Bytes::from_static(&[0x01, 0x02, 0x03])))
        .insert_returning(|row| row)
        .fetch_one()
        .await
        .expect("insert bytes-crate row");
    assert_eq!(inserted.payload, payload);
    assert_eq!(
        inserted.maybe,
        Some(bytes::Bytes::from_static(&[0x01, 0x02, 0x03]))
    );

    // A NULL for the nullable column, then read both rows back as `bytes::Bytes`.
    connection
        .to::<IntegrationBytesCrate>()
        .payload(bytes::Bytes::new())
        .maybe(None)
        .insert()
        .await
        .expect("insert null-maybe row");

    let rows: Vec<(bytes::Bytes, Option<bytes::Bytes>)> = connection
        .from::<IntegrationBytesCrate>()
        .order_by(|(row,)| row.id.asc())
        .select(|(row,)| (row.payload, row.maybe))
        .collect()
        .await
        .expect("select bytes-crate rows");
    assert_eq!(
        rows,
        vec![
            (
                payload,
                Some(bytes::Bytes::from_static(&[0x01, 0x02, 0x03]))
            ),
            (bytes::Bytes::new(), None),
        ]
    );
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

#[tokio::test]
#[ignore]
async fn postgres_window_functions_round_trip() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS join_posts; DROP TABLE IF EXISTS join_users")
        .await
        .expect("drop old join tables");

    let ddl_backend = Postgres;
    create_table::<JoinUser>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);
    connection
        .to::<JoinUser>()
        .name("Ada")
        .insert()
        .await
        .expect("insert Ada");
    connection
        .to::<JoinUser>()
        .name("Grace")
        .insert()
        .await
        .expect("insert Grace");

    // ROW_NUMBER() over the users ordered by id, decoded end to end.
    let ranked = connection
        .from::<JoinUser>()
        .select(|(user,)| (user.name, row_number().over(|w| w.order_by(user.id.asc()))))
        .collect()
        .await
        .expect("fetch window row numbers");
    assert_eq!(
        ranked,
        vec![("Ada".to_owned(), 1_i64), ("Grace".to_owned(), 2_i64)]
    );
}

#[tokio::test]
#[ignore]
async fn postgres_distinct_deduplicates_rows() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_users")
        .await
        .expect("drop old integration table");

    let ddl_backend = Postgres;
    create_table::<IntegrationUser>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    for name in ["dup", "dup", "unique"] {
        connection
            .to::<IntegrationUser>()
            .name(name)
            .insert()
            .await
            .expect("insert row");
    }

    let names = connection
        .from::<IntegrationUser>()
        .distinct()
        .order_by(|(user,)| user.name.asc())
        .select(|(user,)| user.name)
        .collect()
        .await
        .expect("fetch distinct names");

    assert_eq!(names, vec!["dup".to_owned(), "unique".to_owned()]);

    // COUNT(DISTINCT name) over {dup, dup, unique} = 2 distinct names (vs COUNT(name) = 3).
    let distinct_names = connection
        .from::<IntegrationUser>()
        .select(|(user,)| user.name.count().distinct())
        .fetch_one()
        .await
        .expect("fetch count distinct");
    assert_eq!(distinct_names, 2_i64);
}

#[tokio::test]
#[ignore]
async fn postgres_bytea_column_round_trips() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_bytess")
        .await
        .expect("drop old bytes table");

    let ddl_backend = Postgres;
    create_table::<IntegrationBytes>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let payload = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 0x00, 0xFF];
    let inserted = connection
        .to::<IntegrationBytes>()
        .data(payload.clone())
        .maybe(Some(vec![0x01, 0x02, 0x03]))
        .insert_returning(|row| row)
        .fetch_one()
        .await
        .expect("insert bytea row");
    assert_eq!(inserted.data, payload);
    assert_eq!(inserted.maybe, Some(vec![0x01, 0x02, 0x03]));

    // A NULL for the nullable bytea column.
    connection
        .to::<IntegrationBytes>()
        .data(vec![])
        .maybe(None)
        .insert()
        .await
        .expect("insert null-bytea row");

    let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = connection
        .from::<IntegrationBytes>()
        .order_by(|(row,)| row.id.asc())
        .select(|(row,)| (row.data, row.maybe))
        .collect()
        .await
        .expect("select bytea rows");
    assert_eq!(
        rows,
        vec![(payload, Some(vec![0x01, 0x02, 0x03])), (Vec::new(), None),]
    );
}

#[tokio::test]
#[ignore]
async fn postgres_fixed_bytes_column_round_trips() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_fixed_bytess")
        .await
        .expect("drop old fixed-bytes table");

    let ddl_backend = Postgres;
    create_table::<IntegrationFixedBytes>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    let key = [0xDEu8, 0xAD, 0xBE, 0xEF];
    let inserted = connection
        .to::<IntegrationFixedBytes>()
        .key(key)
        .nonce(Some([0x01, 0x02]))
        .insert_returning(|row| row)
        .fetch_one()
        .await
        .expect("insert fixed-bytes row");
    assert_eq!(inserted.key, key);
    assert_eq!(inserted.nonce, Some([0x01, 0x02]));

    // A NULL for the nullable fixed-bytes column.
    connection
        .to::<IntegrationFixedBytes>()
        .key([0, 0, 0, 0])
        .nonce(None)
        .insert()
        .await
        .expect("insert null-nonce row");

    let rows: Vec<([u8; 4], Option<[u8; 2]>)> = connection
        .from::<IntegrationFixedBytes>()
        .order_by(|(row,)| row.id.asc())
        .select(|(row,)| (row.key, row.nonce))
        .collect()
        .await
        .expect("select fixed-bytes rows");
    assert_eq!(rows, vec![(key, Some([0x01, 0x02])), ([0, 0, 0, 0], None)]);
}

#[tokio::test]
#[ignore]
async fn postgres_fixed_bytes_width_is_enforced() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_fixed_bytess")
        .await
        .expect("drop old fixed-bytes table");

    create_table::<IntegrationFixedBytes>(&client, &Postgres).await;

    // The generated `octet_length` CHECK rejects a wrong-width insert at the database.
    let rejected = client
        .batch_execute("INSERT INTO integration_fixed_bytess (key) VALUES ('\\x0102')")
        .await;
    assert!(
        rejected.is_err(),
        "a 2-byte value must violate the octet_length(key) = 4 CHECK"
    );

    // Drop the generated check, store a wrong-width value directly, then confirm the typed decode
    // rejects it (the length check is enforced on read as well as by the DDL constraint).
    // Scope to the `key` column's check: `nonce` is also fixed-width, so the table has two checks.
    let conname: String = client
        .query_one(
            "SELECT conname FROM pg_constraint \
             WHERE conrelid = 'integration_fixed_bytess'::regclass AND contype = 'c' \
             AND pg_get_constraintdef(oid) LIKE '%octet_length(key)%'",
            &[],
        )
        .await
        .expect("find generated check constraint")
        .get(0);
    client
        .batch_execute(&format!(
            "ALTER TABLE integration_fixed_bytess DROP CONSTRAINT \"{conname}\""
        ))
        .await
        .expect("drop generated check");
    client
        .batch_execute("INSERT INTO integration_fixed_bytess (key) VALUES ('\\x0102')")
        .await
        .expect("insert wrong-width value after dropping the check");

    let connection = PostgresConnection::new(client);
    let result: Result<Vec<(i32, [u8; 4])>, _> = connection
        .from::<IntegrationFixedBytes>()
        .select(|(row,)| (row.id, row.key))
        .collect()
        .await;
    assert!(
        result.is_err(),
        "decoding a 2-byte value into [u8; 4] must error"
    );
}

#[tokio::test]
#[ignore]
async fn postgres_case_expression_round_trips() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_users")
        .await
        .expect("drop old integration table");

    let ddl_backend = Postgres;
    create_table::<IntegrationUser>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);
    for name in ["Ada", "Grace"] {
        connection
            .to::<IntegrationUser>()
            .name(name)
            .insert()
            .await
            .expect("insert user");
    }

    // CASE WHEN name = 'Ada' THEN 100 ELSE 0 END, ordered by id.
    let labels: Vec<i32> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| case().when(user.name.equals("Ada"), 100).otherwise(0))
        .collect()
        .await
        .expect("fetch case labels");
    assert_eq!(labels, vec![100, 0]);

    // Without ELSE the result is nullable.
    let nullable: Vec<Option<i32>> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| case().when(user.name.equals("Ada"), 100).end())
        .collect()
        .await
        .expect("fetch nullable case labels");
    assert_eq!(nullable, vec![Some(100), None]);
}

#[tokio::test]
#[ignore]
async fn postgres_coalesce_nullif_simple_case_round_trip() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_users")
        .await
        .expect("drop old integration table");

    let ddl_backend = Postgres;
    create_table::<IntegrationUser>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);
    for name in ["Ada", "Grace"] {
        connection
            .to::<IntegrationUser>()
            .name(name)
            .insert()
            .await
            .expect("insert user");
    }

    // NULLIF(id, 1): NULL for Ada (id 1), else id. The per-operand CAST makes the bound `1` typeable.
    let nullif_vals: Vec<Option<i32>> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| nullif(user.id, 1))
        .collect()
        .await
        .expect("fetch nullif");
    assert_eq!(nullif_vals, vec![None, Some(2)]);

    // COALESCE(id, 999): id is non-null, so the result is non-null and returns id.
    let coalesce_vals: Vec<i32> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| coalesce(user.id).or_else(999).end())
        .collect()
        .await
        .expect("fetch coalesce");
    assert_eq!(coalesce_vals, vec![1, 2]);

    // Simple CASE id WHEN 1 THEN 100 ELSE 0 END — all-parameter THEN/ELSE typed by the per-branch CAST.
    let simple_vals: Vec<i32> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| case_of(user.id).when(1, 100).otherwise(0))
        .collect()
        .await
        .expect("fetch simple case");
    assert_eq!(simple_vals, vec![100, 0]);
}

#[tokio::test]
#[ignore]
async fn postgres_string_functions_round_trip() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_users")
        .await
        .expect("drop old integration table");

    let ddl_backend = Postgres;
    create_table::<IntegrationUser>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);
    for name in ["Ada", "Grace"] {
        connection
            .to::<IntegrationUser>()
            .name(name)
            .insert()
            .await
            .expect("insert user");
    }

    let lowered: Vec<String> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| lower(user.name))
        .collect()
        .await
        .expect("fetch lower");
    assert_eq!(lowered, vec!["ada".to_owned(), "grace".to_owned()]);

    let lengths: Vec<i32> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| length(user.name))
        .collect()
        .await
        .expect("fetch length");
    assert_eq!(lengths, vec![3, 5]);

    let greetings: Vec<String> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user.name.concat("!"))
        .collect()
        .await
        .expect("fetch concat");
    assert_eq!(greetings, vec!["Ada!".to_owned(), "Grace!".to_owned()]);

    let prefixes: Vec<String> = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| substring(user.name, 1, 2))
        .collect()
        .await
        .expect("fetch substring");
    assert_eq!(prefixes, vec!["Ad".to_owned(), "Gr".to_owned()]);
}

#[cfg(feature = "systemtime")]
#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationTimed<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    created: C::Type<'scope, std::time::SystemTime>,
}

#[cfg(feature = "systemtime")]
#[tokio::test]
#[ignore]
async fn postgres_datetime_functions_round_trip() {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_timeds")
        .await
        .expect("drop old integration table");

    let ddl_backend = Postgres;
    create_table::<IntegrationTimed>(&client, &ddl_backend).await;

    // 2021-01-01 12:34:56 UTC and 2022-01-01 12:34:56 UTC (whole seconds round-trip exactly).
    let t1 = UNIX_EPOCH + Duration::from_secs(1_609_504_496);
    let t2 = UNIX_EPOCH + Duration::from_secs(1_641_040_496);
    // The same instants truncated to the day (midnight UTC).
    let day1 = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
    let day2 = UNIX_EPOCH + Duration::from_secs(1_640_995_200);

    let connection = PostgresConnection::new(client);
    for ts in [t1, t2] {
        connection
            .to::<IntegrationTimed>()
            .created(ts)
            .insert()
            .await
            .expect("insert timed row");
    }

    // `extract_at(YEAR, ..., "UTC")` -> bigint, pinned to UTC so the assertion holds regardless of the
    // connection's session `TimeZone`.
    let years: Vec<i64> = connection
        .from::<IntegrationTimed>()
        .order_by(|(e,)| e.id.asc())
        .select(|(e,)| extract_at(DateField::Year, e.created, "UTC"))
        .collect()
        .await
        .expect("fetch extract year");
    assert_eq!(years, vec![2021, 2022]);

    // `date_trunc_at('day', ..., "UTC")` -> the same timestamp type, truncated to UTC midnight (the
    // session-zone-dependent bare `date_trunc` would truncate to local midnight).
    let truncated: Vec<SystemTime> = connection
        .from::<IntegrationTimed>()
        .order_by(|(e,)| e.id.asc())
        .select(|(e,)| date_trunc_at(DateField::Day, e.created, "UTC"))
        .collect()
        .await
        .expect("fetch date_trunc day");
    assert_eq!(truncated, vec![day1, day2]);

    // Non-UTC zone: truncating `2021-01-01 12:34:56Z` at `America/New_York` (UTC-5 in January) is
    // `2021-01-01 00:00 EST` = `2021-01-01 05:00:00Z` — the New York midnight *instant*, not UTC
    // midnight. This is the case the UTC-only assertions above mask (the `AT TIME ZONE` round-trip
    // returns a `timestamptz`, so the decoded instant is correct).
    let ny_midnight = UNIX_EPOCH + Duration::from_secs(1_609_459_200 + 5 * 3600);
    let ny_truncated: Vec<SystemTime> = connection
        .from::<IntegrationTimed>()
        .where_(|e| e.id.equals(1))
        .select(|(e,)| date_trunc_at(DateField::Day, e.created, "America/New_York"))
        .collect()
        .await
        .expect("fetch date_trunc day at America/New_York");
    assert_eq!(ny_truncated, vec![ny_midnight]);

    // DST fall-back ambiguity: 2021-11-07 05:30:00Z is 01:30 EDT (the *first* occurrence of the
    // repeated 1 a.m. hour in New York). Truncating to the hour must yield 01:00 EDT = 05:00:00Z, not
    // 01:00 EST = 06:00:00Z. PostgreSQL's 3-arg `date_trunc` resolves this correctly; an `AT TIME ZONE`
    // round-trip would not.
    let dst_instant = UNIX_EPOCH + Duration::from_secs(1_636_263_000); // 2021-11-07 05:30:00Z
    let dst_hour = UNIX_EPOCH + Duration::from_secs(1_636_261_200); // 2021-11-07 05:00:00Z
    let dst_truncated: Vec<SystemTime> = connection
        .from::<IntegrationTimed>()
        .where_(|e| e.id.equals(1))
        .select(|(_e,)| date_trunc_at(DateField::Hour, dst_instant, "America/New_York"))
        .collect()
        .await
        .expect("fetch date_trunc hour across DST fall-back");
    assert_eq!(dst_truncated, vec![dst_hour]);

    // `now()` -> the current transaction timestamp, which is after the inserted rows.
    let nows: Vec<SystemTime> = connection
        .from::<IntegrationTimed>()
        .select(|(_e,)| now::<SystemTime>())
        .collect()
        .await
        .expect("fetch now");
    assert_eq!(nows.len(), 2);
    assert!(nows.iter().all(|n| *n > t2), "now() should be after 2022");

    // Second / sub-second over a fractional instant (2021-01-01 00:00:56.789Z): `extract(Second)` is
    // the whole-seconds component (`56`), `extract_second` keeps the fraction (`56.789`).
    let frac = UNIX_EPOCH + Duration::from_secs(1_609_459_256) + Duration::from_millis(789);
    let whole_seconds: Vec<i64> = connection
        .from::<IntegrationTimed>()
        .where_(|e| e.id.equals(1))
        .select(|(_e,)| extract(DateField::Second, frac))
        .collect()
        .await
        .expect("fetch extract second");
    assert_eq!(whole_seconds, vec![56]);

    let frac_seconds: Vec<f64> = connection
        .from::<IntegrationTimed>()
        .where_(|e| e.id.equals(1))
        .select(|(_e,)| extract_second(frac))
        .collect()
        .await
        .expect("fetch extract_second");
    assert_eq!(frac_seconds.len(), 1);
    assert!(
        (frac_seconds[0] - 56.789).abs() < 1e-6,
        "extract_second = {}",
        frac_seconds[0]
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
struct IntegrationUpsert<'scope, C: ColumnMode = ColumnExpr> {
    // A non-auto primary key, so the conflict target can be set explicitly.
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[tokio::test]
#[ignore]
async fn postgres_upsert_on_conflict_round_trip() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_upserts")
        .await
        .expect("drop old upsert table");

    let ddl_backend = Postgres;
    create_table::<IntegrationUpsert>(&client, &ddl_backend).await;

    let connection = PostgresConnection::new(client);

    // Seed (id=1, "Ada").
    connection
        .to::<IntegrationUpsert>()
        .id(1)
        .name("Ada")
        .insert()
        .await
        .expect("seed insert");

    // DO NOTHING on the same key leaves the existing row unchanged.
    connection
        .to::<IntegrationUpsert>()
        .id(1)
        .name("Grace")
        .on_conflict(|u| u.id)
        .do_nothing()
        .insert()
        .await
        .expect("upsert do_nothing");
    let after_nothing: Vec<String> = connection
        .from::<IntegrationUpsert>()
        .where_(|u| u.id.equals(1))
        .select(|(u,)| u.name)
        .collect()
        .await
        .expect("fetch after do_nothing");
    assert_eq!(after_nothing, vec!["Ada".to_owned()]);

    // DO UPDATE replaces the row with the proposed (EXCLUDED) values, and RETURNING yields it.
    let returned = connection
        .to::<IntegrationUpsert>()
        .id(1)
        .name("Grace")
        .on_conflict(|u| u.id)
        .do_update()
        .insert_returning(|u| u.name)
        .fetch_one()
        .await
        .expect("upsert do_update");
    assert_eq!(returned, "Grace");

    let after_update: Vec<String> = connection
        .from::<IntegrationUpsert>()
        .where_(|u| u.id.equals(1))
        .select(|(u,)| u.name)
        .collect()
        .await
        .expect("fetch after do_update");
    assert_eq!(after_update, vec!["Grace".to_owned()]);
}

#[tokio::test]
#[ignore]
async fn postgres_insert_select_round_trip() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS integration_users")
        .await
        .expect("drop old integration table");
    create_table::<IntegrationUser>(&client, &Postgres).await;
    let connection = PostgresConnection::new(client);

    connection
        .to::<IntegrationUser>()
        .name("Ada")
        .insert()
        .await
        .expect("seed Ada");

    // INSERT INTO integration_users (name) SELECT name FROM integration_users — duplicates each row.
    let affected = connection
        .to::<IntegrationUser>()
        .insert_select(
            |user| user.name,
            connection
                .from::<IntegrationUser>()
                .select(|(user,)| user.name),
        )
        .insert()
        .await
        .expect("insert ... select");
    assert_eq!(affected, 1);

    let names = connection
        .from::<IntegrationUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user.name)
        .collect()
        .await
        .expect("select after insert-select");
    assert_eq!(names, vec!["Ada".to_owned(), "Ada".to_owned()]);
}

#[tokio::test]
#[ignore]
async fn postgres_update_from_round_trip() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS join_posts; DROP TABLE IF EXISTS join_users")
        .await
        .expect("drop old join tables");
    create_table::<JoinUser>(&client, &Postgres).await;
    create_table::<JoinPost>(&client, &Postgres).await;
    let connection = PostgresConnection::new(client);

    let ada = connection
        .to::<JoinUser>()
        .name("Ada")
        .insert_returning(|user| user)
        .fetch_one()
        .await
        .expect("insert Ada");
    connection
        .to::<JoinPost>()
        .user_id(ada.id)
        .title("Renamed")
        .insert()
        .await
        .expect("insert post");

    // UPDATE join_users SET name = p.title FROM join_posts p WHERE join_users.id = p.user_id.
    let affected = connection
        .to_columns::<JoinUser, (JoinUserName,)>()
        .from::<JoinPost>()
        .set(|(_user, post)| (post.title,))
        .where_(|(user, post)| user.id.equals(post.user_id))
        .update()
        .await
        .expect("update ... from");
    assert_eq!(affected, 1);

    let names = connection
        .from::<JoinUser>()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user.name)
        .collect()
        .await
        .expect("select after update-from");
    assert_eq!(names, vec!["Renamed".to_owned()]);
}

#[tokio::test]
#[ignore]
async fn postgres_delete_using_round_trip() {
    let _db_guard = db_lock().lock().await;
    let client = connect().await;
    client
        .batch_execute("DROP TABLE IF EXISTS join_posts; DROP TABLE IF EXISTS join_users")
        .await
        .expect("drop old join tables");
    create_table::<JoinUser>(&client, &Postgres).await;
    create_table::<JoinPost>(&client, &Postgres).await;
    let connection = PostgresConnection::new(client);

    let ada = connection
        .to::<JoinUser>()
        .name("Ada")
        .insert_returning(|user| user)
        .fetch_one()
        .await
        .expect("insert Ada");
    let grace = connection
        .to::<JoinUser>()
        .name("Grace")
        .insert_returning(|user| user)
        .fetch_one()
        .await
        .expect("insert Grace");
    connection
        .to::<JoinPost>()
        .user_id(ada.id)
        .title("Ada's post")
        .insert()
        .await
        .expect("insert Ada's post");
    connection
        .to::<JoinPost>()
        .user_id(grace.id)
        .title("Grace's post")
        .insert()
        .await
        .expect("insert Grace's post");

    // DELETE join_posts p USING join_users u WHERE p.user_id = u.id AND u.name = 'Ada' — removes the
    // posts authored by Ada. Deleting the child side keeps the `join_posts → join_users` FK satisfied.
    let affected = connection
        .from::<JoinPost>()
        .using::<JoinUser>()
        .where_(|(post, user)| post.user_id.equals(user.id).and(user.name.equals("Ada")))
        .delete()
        .await
        .expect("delete ... using");
    assert_eq!(affected, 1);

    let titles = connection
        .from::<JoinPost>()
        .order_by(|(post,)| post.id.asc())
        .select(|(post,)| post.title)
        .collect()
        .await
        .expect("select after delete-using");
    assert_eq!(titles, vec!["Grace's post".to_owned()]);
}
