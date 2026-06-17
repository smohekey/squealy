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

#[test]
fn mysql_ilike_falls_back_to_plain_like() {
    // MySQL has no `ILIKE`; its default collations make `LIKE` case-insensitive, so the dialect
    // default maps `ilike` to a plain `LIKE`.
    let query = Mysql
        .from::<Widget>()
        .where_(|widget| widget.name.ilike("a%"))
        .select(|(widget,)| widget.id);
    let sql = query.to_sql();
    assert!(sql.contains("LIKE ?"), "expected a plain LIKE: {sql}");
    assert!(!sql.contains("ILIKE"), "MySQL has no ILIKE: {sql}");
}

#[test]
fn mysql_in_and_between_render() {
    let in_query = Mysql
        .from::<Widget>()
        .where_(|widget| widget.id.in_([1, 2, 3]))
        .select(|(widget,)| widget.id);
    let sql = in_query.to_sql();
    assert!(sql.contains("IN (?, ?, ?)"), "{sql}");

    let between_query = Mysql
        .from::<Widget>()
        .where_(|widget| widget.id.between(1, 10))
        .select(|(widget,)| widget.id);
    let sql = between_query.to_sql();
    assert!(sql.contains("BETWEEN ? AND ?"), "{sql}");
}

#[test]
fn mysql_sum_and_avg_cast_in_its_dialect() {
    // The aggregate cast uses MySQL's restricted CAST vocabulary: `SIGNED` for the widened integer
    // `SUM`, `DECIMAL` for `AVG`. `COUNT` needs no cast.
    let sum = Mysql.from::<Widget>().select(|(widget,)| widget.id.sum());
    assert!(
        sum.to_sql().contains("CAST(SUM(q0_0.`id`) AS SIGNED)"),
        "{}",
        sum.to_sql()
    );

    let avg = Mysql.from::<Widget>().select(|(widget,)| widget.id.avg());
    assert!(
        avg.to_sql().contains("CAST(AVG(q0_0.`id`) AS DECIMAL)"),
        "{}",
        avg.to_sql()
    );

    let count = Mysql.from::<Widget>().select(|(widget,)| widget.id.count());
    assert!(
        count.to_sql().contains("COUNT(q0_0.`id`)"),
        "{}",
        count.to_sql()
    );
    assert!(!count.to_sql().contains("CAST"), "{}", count.to_sql());
}
