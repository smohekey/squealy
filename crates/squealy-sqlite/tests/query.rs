//! SQLite query rendering through the shared core renderer + `SqliteDialect`.
//!
//! This slice is driver-free, so the tests only assert `to_sql()` / `collect_params()` output — no
//! execution. They cover the reachable render paths: a filtered `SELECT`, an `INSERT` (via the upsert
//! `build()` inspection path, since SQLite advertises no `RETURNING` in this slice), a correlated
//! `UPDATE … FROM`, a correlated `DELETE … USING`, a `UNION` set operation, and the SQLite-specific
//! `length()` spelling of the character-length scalar function.

use squealy::*;
use squealy_sqlite::{Sqlite, SqliteValue};

#[derive(Clone, Debug, PartialEq, Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    count: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Gadget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    widget_id: C::Type<'scope, i32>,
}

#[test]
fn sqlite_renders_select_with_where_in_its_dialect() {
    let query = Sqlite
        .from::<Widget>()
        .where_(|widget| widget.name.equals("Ada"))
        .select(|(widget,)| (widget.id, widget.name));

    let sql = query.to_sql();

    // Double-quoted identifiers and a positional `?` placeholder (not Postgres `$1`, not MySQL backticks).
    assert!(
        sql.contains("\"widgets\""),
        "expected double-quoted table: {sql}"
    );
    assert!(sql.contains('?'), "expected a `?` placeholder: {sql}");
    assert!(
        !sql.contains("$1"),
        "must not use Postgres placeholders: {sql}"
    );
    assert!(!sql.contains('`'), "must not use MySQL backticks: {sql}");

    assert_eq!(
        query.collect_params().unwrap(),
        vec![SqliteValue::Text("Ada".to_owned())]
    );
}

#[test]
fn sqlite_renders_insert() {
    // A plain insert is only inspectable (driver-free, no `RETURNING`) through the upsert `build()`
    // path; `do_nothing()` with no conflict column keeps the `INSERT` otherwise plain.
    let insert = Sqlite
        .to::<Widget>()
        .name("Ada")
        .count(0)
        .on_conflict(|widget| widget.id)
        .do_nothing()
        .build();

    let sql = insert.to_sql();
    assert!(
        sql.starts_with("INSERT INTO \"widgets\" (\"name\", \"count\") VALUES (?, ?)"),
        "{sql}"
    );
    assert!(
        sql.contains("ON CONFLICT (\"id\") DO NOTHING"),
        "expected an ON CONFLICT clause: {sql}"
    );
    assert_eq!(
        insert.collect_params().unwrap(),
        vec![SqliteValue::Text("Ada".to_owned()), SqliteValue::Integer(0)]
    );
}

#[test]
fn sqlite_renders_correlated_update() {
    // SQLite renders a correlated update as `UPDATE t AS a SET … FROM other AS b WHERE <correlation>`.
    let update = Sqlite
        .to_columns::<Widget, (WidgetCount,)>()
        .from::<Gadget>()
        .set(|(_widget, gadget)| (gadget.widget_id,))
        .where_(|(widget, gadget)| widget.id.equals(gadget.id))
        .build();

    let sql = update.to_sql();
    assert!(sql.starts_with("UPDATE \"widgets\" AS "), "{sql}");
    assert!(sql.contains("SET \"count\" = "), "{sql}");
    assert!(sql.contains("FROM \"gadgets\" AS "), "{sql}");
    assert!(!sql.contains('`'), "must not use MySQL backticks: {sql}");
    assert_eq!(update.collect_params().unwrap(), Vec::<SqliteValue>::new());
}

#[test]
fn sqlite_renders_correlated_delete() {
    // SQLite renders a correlated delete as `DELETE FROM t AS a USING other AS b WHERE <correlation>`.
    let delete = Sqlite
        .from::<Widget>()
        .using::<Gadget>()
        .where_(|(widget, gadget)| widget.id.equals(gadget.widget_id))
        .build();

    let sql = delete.to_sql();
    // SQLite has no join-delete, so a correlated delete is a correlated EXISTS subquery.
    assert!(sql.starts_with("DELETE FROM \"widgets\" AS "), "{sql}");
    assert!(
        sql.contains("WHERE EXISTS (SELECT 1 FROM \"gadgets\" AS "),
        "expected an EXISTS-subquery correlated delete: {sql}"
    );
    assert!(
        !sql.contains("USING"),
        "SQLite has no DELETE … USING: {sql}"
    );
    assert!(!sql.contains('`'), "must not use MySQL backticks: {sql}");
    assert_eq!(delete.collect_params().unwrap(), Vec::<SqliteValue>::new());
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    handle: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    accounts: Account<'static, ColumnName>,
}

#[test]
fn sqlite_suppresses_schema_qualification() {
    // A `#[schema(Public)]` table renders unqualified for SQLite (which has no schemas), matching how
    // its DDL flattens schemas — not `"public"."accounts"`, which SQLite would read as a database name.
    let query = Sqlite.from::<Account>().select(|(account,)| account.id);
    let sql = query.to_sql();
    assert!(sql.contains("FROM \"accounts\""), "{sql}");
    assert!(
        !sql.contains("\"public\""),
        "must not qualify with the schema: {sql}"
    );
}

#[test]
fn sqlite_substring_renders_as_substr_function_call() {
    // SQLite has no `SUBSTRING(s FROM start FOR len)`; it uses the `substr(s, start, len)` call.
    let query = Sqlite
        .from::<Widget>()
        .select(|(widget,)| substring(widget.name, 2, 3));
    let sql = query.to_sql();
    assert!(sql.contains("substr("), "expected substr(...): {sql}");
    assert!(
        !sql.contains("SUBSTRING"),
        "SQLite must not use SUBSTRING: {sql}"
    );
    // The `FROM start FOR len` substring syntax is gone (the ` FOR ` keyword is unique to it).
    assert!(
        !sql.contains(" FOR "),
        "no FROM/FOR substring syntax: {sql}"
    );
}

#[test]
fn sqlite_renders_union_set_operation() {
    let union = Sqlite
        .from::<Widget>()
        .select(|(widget,)| (widget.id, widget.name))
        .union(
            Sqlite
                .from::<Widget>()
                .where_(|widget| widget.name.equals("Ada"))
                .select(|(widget,)| (widget.id, widget.name)),
        );

    let sql = union.to_sql();
    // SQLite renders set operands bare (no parenthesized `(SELECT …)`, which it rejects).
    assert!(
        sql.contains(" UNION SELECT "),
        "expected a bare UNION: {sql}"
    );
    assert!(
        !sql.contains(") UNION ("),
        "SQLite must not parenthesize set operands: {sql}"
    );
    assert!(
        sql.contains("\"widgets\""),
        "expected double-quoted identifiers: {sql}"
    );
    assert!(!sql.contains('`'), "must not use MySQL backticks: {sql}");
    assert_eq!(
        union.collect_params().unwrap(),
        vec![SqliteValue::Text("Ada".to_owned())]
    );
}

#[test]
fn sqlite_length_renders_as_length_not_char_length() {
    // SQLite has no `CHAR_LENGTH`; the dialect maps the character-length scalar to `length(...)`.
    let query = Sqlite
        .from::<Widget>()
        .select(|(widget,)| length(widget.name));

    let sql = query.to_sql();
    assert!(
        sql.contains("length(") && sql.contains("\"name\")"),
        "expected length(…\"name\"): {sql}"
    );
    assert!(
        !sql.contains("CHAR_LENGTH"),
        "SQLite must not render CHAR_LENGTH: {sql}"
    );
}
