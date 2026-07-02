//! Live SQLite round-trips through the runtime: render + execute against an in-memory database.
//!
//! Unlike the MySQL/PostgreSQL integration tests (which need a server and are `#[ignore]`d), SQLite
//! runs in-process, so these execute in the normal `cargo test` run — no external database required.
//! Each test opens its own private `:memory:` connection.

use squealy::*;
use squealy_sqlite::{Sqlite, SqliteConnection, SqliteError};

#[derive(Clone, Debug, PartialEq, Table)]
struct RuntimeWidget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    count: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct RuntimeGadget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    widget_id: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct RuntimeAccount<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    slug: C::Type<'scope, String>,
    label: C::Type<'scope, String>,
}

async fn connect() -> SqliteConnection {
    Sqlite.connect(":memory:").await.expect("open in-memory db")
}

#[tokio::test]
async fn round_trips_insert_and_select() {
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE runtime_widgets (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, count INTEGER NOT NULL)",
        )
        .await
        .expect("create table");

    // Insert two rows through the runtime (rendered with `?` placeholders, executed via SqliteRows).
    let affected = connection
        .to::<RuntimeWidget>()
        .name("gadget")
        .count(1)
        .insert()
        .await
        .expect("insert gadget");
    assert_eq!(affected, 1);
    connection
        .to::<RuntimeWidget>()
        .name("widget")
        .count(2)
        .insert()
        .await
        .expect("insert widget");

    // Filtered select, decoding a single scalar column through the value codec.
    let names = connection
        .from::<RuntimeWidget>()
        .where_(|widget| widget.name.equals("gadget"))
        .select(|(widget,)| widget.name)
        .collect()
        .await
        .expect("select gadget");
    assert_eq!(names, vec!["gadget".to_owned()]);

    // Ordered select of a tuple, exercising integer + text decoding and generated-id round-trip.
    let all = connection
        .from::<RuntimeWidget>()
        .order_by(|(widget,)| widget.id.asc())
        .select(|(widget,)| (widget.id, widget.name, widget.count))
        .collect()
        .await
        .expect("select all");
    assert_eq!(
        all,
        vec![(1, "gadget".to_owned(), 1), (2, "widget".to_owned(), 2),]
    );
}

#[tokio::test]
async fn returning_yields_inserted_and_updated_rows() {
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE runtime_accounts (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, label TEXT NOT NULL)",
        )
        .await
        .expect("create table");

    // INSERT … RETURNING hands back the generated id alongside the inserted label.
    let (id, label) = connection
        .to::<RuntimeAccount>()
        .slug("acme")
        .label("first")
        .insert_returning(|account| (account.id, account.label))
        .fetch_one()
        .await
        .expect("insert returning");
    assert_eq!(id, 1);
    assert_eq!(label, "first");

    // UPDATE … RETURNING hands back the post-update value.
    let updated = connection
        .to_columns::<RuntimeAccount, (RuntimeAccountLabel,)>()
        .set(|_account| ("second",))
        .where_(|account| account.id.equals(id))
        .update_returning(|account| account.label)
        .fetch_one()
        .await
        .expect("update returning");
    assert_eq!(updated, "second");

    // Invoking a RETURNING mutation via `.execute()` (instead of fetch) drains the returned rows and
    // still reports the affected count — `rusqlite::Connection::execute` alone would reject the rows.
    let affected = connection
        .to::<RuntimeAccount>()
        .slug("beta")
        .label("b")
        .insert_returning(|account| account.id)
        .execute()
        .await
        .expect("execute returning insert");
    assert_eq!(affected, 1);
}

#[tokio::test]
async fn update_and_delete_report_affected_counts() {
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE runtime_widgets (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, count INTEGER NOT NULL)",
        )
        .await
        .expect("create table");
    for name in ["a", "b", "c"] {
        connection
            .to::<RuntimeWidget>()
            .name(name)
            .count(0)
            .insert()
            .await
            .expect("insert");
    }

    let updated = connection
        .to_columns::<RuntimeWidget, (RuntimeWidgetCount,)>()
        .set(|_widget| (9,))
        .where_(|widget| widget.name.equals("b"))
        .update()
        .await
        .expect("update");
    assert_eq!(updated, 1);

    let deleted = connection
        .from::<RuntimeWidget>()
        .where_(|widget| widget.count.equals(0))
        .delete()
        .await
        .expect("delete");
    assert_eq!(deleted, 2);

    let remaining = connection
        .from::<RuntimeWidget>()
        .select(|(widget,)| widget.name)
        .collect()
        .await
        .expect("select remaining");
    assert_eq!(remaining, vec!["b".to_owned()]);
}

#[tokio::test]
async fn upsert_on_conflict_updates_or_leaves_row() {
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE runtime_accounts (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, label TEXT NOT NULL)",
        )
        .await
        .expect("create table");

    async fn label(connection: &SqliteConnection) -> Vec<String> {
        connection
            .from::<RuntimeAccount>()
            .where_(|account| account.slug.equals("acme"))
            .select(|(account,)| account.label)
            .collect()
            .await
            .expect("select label")
    }

    connection
        .to::<RuntimeAccount>()
        .slug("acme")
        .label("first")
        .insert()
        .await
        .expect("initial insert");
    assert_eq!(label(&connection).await, vec!["first".to_owned()]);

    // `do_update` on the conflicting `slug` replaces the inserted columns with the proposed values.
    connection
        .to::<RuntimeAccount>()
        .slug("acme")
        .label("second")
        .on_conflict(|account| account.slug)
        .do_update()
        .insert()
        .await
        .expect("upsert do_update");
    assert_eq!(label(&connection).await, vec!["second".to_owned()]);

    // `do_nothing` on the conflict leaves the existing row unchanged.
    connection
        .to::<RuntimeAccount>()
        .slug("acme")
        .label("third")
        .on_conflict(|account| account.slug)
        .do_nothing()
        .insert()
        .await
        .expect("upsert do_nothing");
    assert_eq!(label(&connection).await, vec!["second".to_owned()]);
}

#[tokio::test]
async fn union_set_operation_collects_rows() {
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE runtime_widgets (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, count INTEGER NOT NULL)",
        )
        .await
        .expect("create table");
    for (name, count) in [("a", 1), ("b", 2)] {
        connection
            .to::<RuntimeWidget>()
            .name(name)
            .count(count)
            .insert()
            .await
            .expect("insert");
    }

    // `UNION` de-duplicates; the overlapping `a` row appears once.
    let mut names = connection
        .from::<RuntimeWidget>()
        .where_(|widget| widget.count.equals(1))
        .select(|(widget,)| widget.name)
        .union(
            connection
                .from::<RuntimeWidget>()
                .select(|(widget,)| widget.name),
        )
        .collect()
        .await
        .expect("union collect");
    names.sort();
    assert_eq!(names, vec!["a".to_owned(), "b".to_owned()]);
}

#[tokio::test]
async fn correlated_update_from_and_delete_using() {
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE runtime_widgets (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, count INTEGER NOT NULL);\
             CREATE TABLE runtime_gadgets (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, widget_id INTEGER NOT NULL)",
        )
        .await
        .expect("create tables");
    for name in ["first", "second"] {
        connection
            .to::<RuntimeWidget>()
            .name(name)
            .count(0)
            .insert()
            .await
            .expect("insert widget");
    }
    // A gadget correlating to widget id 1, carrying the value to copy into that widget's `count`.
    connection
        .to::<RuntimeGadget>()
        .widget_id(7)
        .insert()
        .await
        .expect("insert gadget");

    // UPDATE … FROM: set widget.count from the correlated gadget row (matched on id).
    let updated = connection
        .to_columns::<RuntimeWidget, (RuntimeWidgetCount,)>()
        .from::<RuntimeGadget>()
        .set(|(_widget, gadget)| (gadget.widget_id,))
        .where_(|(widget, gadget)| widget.id.equals(gadget.id))
        .update()
        .await
        .expect("correlated update");
    assert_eq!(updated, 1);
    let counts = connection
        .from::<RuntimeWidget>()
        .order_by(|(widget,)| widget.id.asc())
        .select(|(widget,)| widget.count)
        .collect()
        .await
        .expect("select counts");
    assert_eq!(counts, vec![7, 0]);

    // DELETE … USING: delete the widget correlated to the gadget (rendered as a correlated EXISTS).
    let deleted = connection
        .from::<RuntimeWidget>()
        .using::<RuntimeGadget>()
        .where_(|(widget, gadget)| widget.id.equals(gadget.widget_id))
        .delete()
        .await
        .expect("correlated delete");
    // No widget has id 7, so nothing matches — a correct, if empty, correlated delete.
    assert_eq!(deleted, 0);
}

#[tokio::test]
async fn transaction_commits_and_rolls_back() {
    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE runtime_widgets (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, count INTEGER NOT NULL)",
        )
        .await
        .expect("create table");

    // The committed transaction persists its insert.
    let committed = connection
        .transaction(async |transaction| {
            transaction
                .to::<RuntimeWidget>()
                .name("committed")
                .count(0)
                .insert()
                .await?;
            Ok(())
        })
        .await;
    assert!(committed.is_ok());

    // A transaction whose body returns `Err` is rolled back — its insert must not persist.
    let rolled_back: Result<(), SqliteError> = connection
        .transaction(async |transaction| {
            transaction
                .to::<RuntimeWidget>()
                .name("rolled back")
                .count(0)
                .insert()
                .await?;
            Err(SqliteError::NoRows)
        })
        .await;
    assert!(matches!(rolled_back, Err(SqliteError::NoRows)));

    let names = connection
        .from::<RuntimeWidget>()
        .select(|(widget,)| widget.name)
        .collect()
        .await
        .expect("select after transactions");
    assert_eq!(names, vec!["committed".to_owned()]);
}

#[tokio::test]
async fn foreign_keys_are_enforced() {
    #[derive(Clone, Debug, PartialEq, Table)]
    struct Author<'scope, C: ColumnMode = ColumnExpr> {
        #[column(primary_key, auto_increment)]
        id: C::Type<'scope, i32>,
        name: C::Type<'scope, String>,
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    struct Book<'scope, C: ColumnMode = ColumnExpr> {
        #[column(primary_key, auto_increment)]
        id: C::Type<'scope, i32>,
        title: C::Type<'scope, String>,
        #[column(references(Author::id))]
        author_id: C::Type<'scope, i32>,
    }

    let mut connection = connect().await;
    connection
        .execute_ddl(
            "CREATE TABLE authors (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL);\
             CREATE TABLE books (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, author_id INTEGER NOT NULL, \
             FOREIGN KEY (author_id) REFERENCES authors (id))",
        )
        .await
        .expect("create tables");

    // Inserting a book that references a non-existent author must fail — `connect` enables
    // `PRAGMA foreign_keys = ON`, without which SQLite would silently accept the dangling reference.
    let result = connection
        .to::<Book>()
        .title("Orphan")
        .author_id(999)
        .insert()
        .await;
    assert!(
        matches!(result, Err(SqliteError::Query(_))),
        "expected a foreign-key violation, got {result:?}"
    );
}
