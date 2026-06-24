//! MySQL query rendering through the shared core renderer + `MysqlDialect`.

use squealy::*;
use squealy_mysql::Mysql;

#[derive(Clone, Debug, PartialEq, Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    hits: C::Type<'scope, u64>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Gadget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    widget_id: C::Type<'scope, i32>,
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
        avg.to_sql().contains("CAST(AVG(q0_0.`id`) AS DOUBLE)"),
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

#[test]
fn mysql_unsigned_sum_casts_to_full_precision_decimal() {
    // A `u64` SUM widens to `i128` (can exceed `i64::MAX`); on MySQL that must cast to a
    // full-precision DECIMAL, not `SIGNED`, to avoid overflow.
    let sum = Mysql
        .from::<Counter>()
        .select(|(counter,)| counter.hits.sum());
    assert!(
        sum.to_sql()
            .contains("CAST(SUM(q0_0.`hits`) AS DECIMAL(65, 0))"),
        "{}",
        sum.to_sql()
    );
}

#[test]
fn mysql_correlated_in_subquery_renders_in_its_dialect() {
    let query = Mysql
        .from::<Widget>()
        .where_correlated(|(widget,), sub| {
            widget.id.in_subquery(
                sub.from::<Gadget>()
                    .where_(|gadget| gadget.widget_id.equals(widget.id))
                    .select_subquery(|(gadget,)| gadget.widget_id),
            )
        })
        .select(|(widget,)| widget.id);

    assert_eq!(
        query.to_sql(),
        "SELECT q0_0.`id` AS `id` FROM `widgets` AS q0_0 WHERE (q0_0.`id` IN (SELECT \
         q1_0.`widget_id` AS `widget_id` FROM `gadgets` AS q1_0 WHERE (q1_0.`widget_id` = q0_0.`id`)))"
    );
}

#[test]
fn mysql_exists_subquery_uses_question_mark_placeholders() {
    let query = Mysql
        .from::<Widget>()
        .where_correlated(|(_widget,), sub| {
            not_exists(
                sub.from::<Gadget>()
                    .where_(|gadget| gadget.widget_id.equals(3))
                    .select_subquery(|(gadget,)| gadget.widget_id),
            )
        })
        .select(|(widget,)| widget.id);

    let sql = query.to_sql();
    assert!(sql.contains("NOT EXISTS (SELECT"), "{sql}");
    assert!(
        sql.contains("= ?)"),
        "expected `?` placeholder in subquery: {sql}"
    );
    assert!(
        !sql.contains("$1"),
        "must not use Postgres placeholders: {sql}"
    );
    assert_eq!(
        query.collect_params().unwrap(),
        vec![mysql_async::Value::Int(3)]
    );
}

#[test]
fn mysql_window_functions_use_question_mark_placeholders() {
    let row_num = Mysql.from::<Widget>().select(|(widget,)| {
        row_number().over(|w| w.partition_by(widget.name).order_by(widget.id.asc()))
    });
    assert_eq!(
        row_num.to_sql(),
        "SELECT ROW_NUMBER() OVER (PARTITION BY q0_0.`name` ORDER BY q0_0.`id` ASC) AS `expr` \
         FROM `widgets` AS q0_0"
    );

    let args = Mysql.from::<Widget>().select(|(widget,)| {
        (
            ntile(4).over(|w| w.order_by(widget.id.asc())),
            lag(widget.id, 1).over(|w| w.order_by(widget.id.asc())),
        )
    });
    let sql = args.to_sql();
    assert!(sql.contains("NTILE(?)"), "{sql}");
    assert!(sql.contains("LAG(q0_0.`id`, ?)"), "{sql}");
    assert!(
        !sql.contains("$1"),
        "must not use Postgres placeholders: {sql}"
    );
    assert_eq!(
        args.collect_params().unwrap(),
        vec![mysql_async::Value::Int(4), mysql_async::Value::Int(1)]
    );
}

#[test]
fn mysql_distinct_renders_after_select() {
    let query = Mysql
        .from::<Widget>()
        .distinct()
        .select(|(widget,)| widget.name);
    let sql = query.to_sql();
    assert!(
        sql.starts_with("SELECT DISTINCT "),
        "expected DISTINCT right after SELECT: {sql}"
    );
    assert!(
        sql.contains("`name`") && sql.contains("`widgets`"),
        "expected backtick-quoted identifiers: {sql}"
    );
}

#[test]
fn mysql_count_distinct_renders_distinct_inside_call() {
    let query = Mysql
        .from::<Widget>()
        .select(|(widget,)| widget.id.count().distinct());
    let sql = query.to_sql();
    assert!(
        sql.contains("COUNT(DISTINCT ") && sql.contains("`id`)"),
        "expected COUNT(DISTINCT …`id`): {sql}"
    );
}

#[test]
fn mysql_right_join_renders_right_join() {
    // RIGHT JOIN is supported on MySQL (FULL JOIN is not — `full_join` won't compile against Mysql).
    let query = Mysql
        .from::<Widget>()
        .right_join::<Gadget>()
        .on(|(widget,), gadget| gadget.widget_id.equals(widget.id))
        .select(|(widget, gadget)| (widget.id, gadget.id));
    let sql = query.to_sql();
    assert!(
        sql.contains("RIGHT JOIN `gadgets`"),
        "expected RIGHT JOIN with backtick quoting: {sql}"
    );
    assert!(!sql.contains("FULL JOIN"), "unexpected FULL JOIN: {sql}");
}

#[test]
fn mysql_case_when_renders_in_its_dialect() {
    let query = Mysql
        .from::<Widget>()
        .select(|(widget,)| case().when(widget.id.greater_than(10), 1).otherwise(0));
    let sql = query.to_sql();
    // Each branch value is cast to the result type (MySQL's dialect cast) so all-parameter branches
    // are typeable.
    assert!(
        sql.contains("CASE WHEN (q0_0.`id` > ?) THEN CAST(? AS ") && sql.contains("END"),
        "{sql}"
    );
}

#[test]
fn mysql_coalesce_nullif_simple_case_render_in_its_dialect() {
    let coalesce_q = Mysql
        .from::<Widget>()
        .select(|(widget,)| coalesce(widget.id).or_else(0).end());
    let sql = coalesce_q.to_sql();
    // A typed column anchors the type, so the literal sibling is not cast.
    assert!(sql.contains("COALESCE(q0_0.`id`, ?)"), "{sql}");

    // An all-parameter COALESCE casts every argument so it stays typeable.
    let coalesce_lits = Mysql
        .from::<Widget>()
        .select(|(_w,)| coalesce(1).or_else(2).end());
    assert!(
        coalesce_lits.to_sql().contains("COALESCE(CAST(? AS ")
            && coalesce_lits.to_sql().contains("), CAST(?"),
        "{}",
        coalesce_lits.to_sql()
    );

    let nullif_q = Mysql
        .from::<Widget>()
        .select(|(widget,)| nullif(widget.id, 0));
    // A typed column anchors the type, so the literal sibling is not cast.
    assert!(
        nullif_q.to_sql().contains("NULLIF(q0_0.`id`, ?)"),
        "{}",
        nullif_q.to_sql()
    );

    let simple_q = Mysql
        .from::<Widget>()
        .select(|(widget,)| case_of(widget.id).when(1, 10).otherwise(0));
    assert!(
        simple_q
            .to_sql()
            .contains("CASE q0_0.`id` WHEN ? THEN CAST(? AS "),
        "{}",
        simple_q.to_sql()
    );
}
