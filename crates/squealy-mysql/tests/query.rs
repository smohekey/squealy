//! MySQL query rendering through the shared core renderer + `MysqlDialect`.

use squealy::*;
use squealy_mysql::Mysql;

#[derive(Clone, Debug, PartialEq, Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[test]
fn mysql_renders_select_in_its_dialect() {
    let query = Mysql
        .from::<Widget>()
        .where_(|widget| widget.name.equals("Ada"))
        .select(|(widget,)| (widget.id, widget.name));

    let sql = query.to_sql();

    // Backtick quoting and a positional `?` placeholder (not Postgres `$1`).
    assert!(
        sql.contains("`widgets`"),
        "expected backtick-quoted table: {sql}"
    );
    assert!(sql.contains('?'), "expected a `?` placeholder: {sql}");
    assert!(
        !sql.contains("$1"),
        "must not use Postgres placeholders: {sql}"
    );

    assert_eq!(
        query.collect_params().unwrap(),
        vec![mysql_async::Value::Bytes(b"Ada".to_vec())]
    );
}

#[test]
fn mysql_offset_without_limit_renders_a_sentinel_limit() {
    let query = Mysql
        .from::<Widget>()
        .offset(5)
        .select(|(widget,)| widget.id);
    let sql = query.to_sql();
    assert!(
        sql.contains("LIMIT 18446744073709551615 OFFSET 5"),
        "MySQL needs a LIMIT for a bare OFFSET: {sql}"
    );
}

#[test]
fn mysql_renders_division_without_a_float_cast() {
    // MySQL `/` is already float division, so the renderer skips the CAST wrapping Postgres needs.
    let query = Mysql.from::<Widget>().select(|(widget,)| widget.id / 2);
    let sql = query.to_sql();
    assert!(!sql.contains("CAST"), "MySQL division needs no cast: {sql}");
    assert!(sql.contains('/'), "{sql}");
}
