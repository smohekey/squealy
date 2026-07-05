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
fn mysql_scalar_subquery_as_comparison_operand() {
    // A scalar subquery used as a comparison operand renders as a parenthesized SELECT, with MySQL
    // backtick-quoted identifiers.
    let query = Mysql
        .from::<Widget>()
        .where_correlated(|(widget,), sub| {
            widget.id.equals(scalar_subquery(
                sub.from::<Gadget>()
                    .where_(|gadget| gadget.widget_id.equals(widget.id))
                    .select_subquery(|(gadget,)| gadget.widget_id),
            ))
        })
        .select(|(widget,)| widget.id);

    assert_eq!(
        query.to_sql(),
        "SELECT q0_0.`id` AS `id` FROM `widgets` AS q0_0 WHERE (q0_0.`id` = (SELECT \
         q1_0.`widget_id` AS `widget_id` FROM `gadgets` AS q1_0 WHERE (q1_0.`widget_id` = q0_0.`id`)))"
    );
}

#[test]
fn mysql_scalar_subquery_in_projection() {
    // A scalar subquery in the projection renders as a parenthesized SELECT in the select list.
    let query = Mysql.from::<Widget>().select_correlated(|(widget,), sub| {
        scalar_subquery(
            sub.from::<Gadget>()
                .where_(|gadget| gadget.widget_id.equals(widget.id))
                .select_subquery(|(gadget,)| gadget.id),
        )
    });

    assert_eq!(
        query.to_sql(),
        "SELECT (SELECT q1_0.`id` AS `id` FROM `gadgets` AS q1_0 WHERE (q1_0.`widget_id` = q0_0.`id`)) \
         AS `expr` FROM `widgets` AS q0_0"
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
fn mysql_window_frame_clause_renders() {
    // MySQL 8.0+ shares the standard frame syntax with PostgreSQL, so the `ROWS BETWEEN … AND …`
    // clause renders identically (only the identifier quoting differs). Literal bounds bind no `?`.
    let running = Mysql.from::<Widget>().select(|(widget,)| {
        widget.id.sum().over(|w| {
            w.partition_by(widget.name)
                .order_by(widget.id.asc())
                .rows(unbounded_preceding(), current_row())
        })
    });
    let sql = running.to_sql();
    assert!(
        sql.contains(
            "OVER (PARTITION BY q0_0.`name` ORDER BY q0_0.`id` ASC \
             ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"
        ),
        "{sql}"
    );
    assert!(running.collect_params().unwrap().is_empty());

    let banded = Mysql.from::<Widget>().select(|(widget,)| {
        widget.id.avg().over(|w| {
            w.order_by(widget.id.asc())
                .range(preceding(2), following(1))
        })
    });
    assert!(
        banded
            .to_sql()
            .contains("RANGE BETWEEN 2 PRECEDING AND 1 FOLLOWING"),
        "{}",
        banded.to_sql()
    );
}

#[test]
fn mysql_named_window_clause_renders() {
    // MySQL 8.0+ shares the named `WINDOW` clause syntax with PostgreSQL (only the identifier quoting
    // differs). One `WINDOW w0 AS (…)` shared by several `OVER w0` references; the definition binds
    // no `?` placeholders.
    let q = Mysql
        .from::<Widget>()
        .window(|(widget,)| {
            Window::new()
                .partition_by(widget.name)
                .order_by(widget.id.asc())
        })
        .select_over(|(widget,), w| (widget.id.sum().over_ref(w), row_number().over_ref(w)));
    let sql = q.to_sql();
    assert!(
        sql.ends_with("WINDOW w0 AS (PARTITION BY q0_0.`name` ORDER BY q0_0.`id` ASC)"),
        "{sql}"
    );
    assert!(sql.contains("SUM(q0_0.`id`) OVER w0"), "{sql}");
    assert!(sql.contains("ROW_NUMBER() OVER w0"), "{sql}");
    assert!(q.collect_params().unwrap().is_empty());
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

/// A `bytes::Bytes` column binds as a `BLOB`/bytes parameter on MySQL, behind the opt-in `bytes`
/// feature (the codec goes through `Vec<u8>`).
#[cfg(feature = "bytes")]
#[test]
fn mysql_bytes_crate_column_binds_in_queries() {
    #[derive(Clone, Debug, PartialEq, Table)]
    struct Packet<'scope, C: ColumnMode = ColumnExpr> {
        #[column(primary_key, auto_increment)]
        id: C::Type<'scope, i32>,
        payload: C::Type<'scope, bytes::Bytes>,
    }

    let query = Mysql
        .from::<Packet>()
        .where_(|packet| {
            packet
                .payload
                .equals(bytes::Bytes::from_static(&[0xCA, 0xFE]))
        })
        .select(|(packet,)| packet.payload);
    assert_eq!(
        query.collect_params().unwrap(),
        vec![mysql_async::Value::Bytes(vec![0xCA, 0xFE])]
    );
}

#[test]
fn mysql_casts_fixed_bytes_expression_as_binary() {
    // A `[u8; N]` expression forces a cast in an all-parameter CASE; it must stay binary (`BINARY`),
    // not fall through to `CHAR` and be coerced through the connection text charset.
    let query = Mysql.from::<Widget>().select(|(widget,)| {
        case()
            .when(widget.id.greater_than(10), [0u8; 4])
            .otherwise([1u8; 4])
    });
    let sql = query.to_sql();
    assert!(sql.contains("CAST(? AS BINARY)"), "{sql}");
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

#[test]
fn mysql_string_functions_render() {
    let q = Mysql
        .from::<Widget>()
        .select(|(widget,)| lower(widget.name));
    assert!(q.to_sql().contains("LOWER(q0_0.`name`)"), "{}", q.to_sql());

    let len = Mysql
        .from::<Widget>()
        .select(|(widget,)| length(widget.name));
    assert!(
        len.to_sql().contains("CHAR_LENGTH(q0_0.`name`)"),
        "{}",
        len.to_sql()
    );

    let sub = Mysql
        .from::<Widget>()
        .select(|(widget,)| substring(widget.name, 1, 3));
    assert!(
        sub.to_sql().contains("SUBSTRING(q0_0.`name` FROM ? FOR ?)"),
        "{}",
        sub.to_sql()
    );

    let cat = Mysql
        .from::<Widget>()
        .select(|(widget,)| widget.name.concat(widget.name));
    assert!(
        cat.to_sql().contains("CONCAT(q0_0.`name`, q0_0.`name`)"),
        "{}",
        cat.to_sql()
    );
}

// ===== date/time functions (feature-gated on the timestamp type) =====

// MySQL has no SystemTime Encode/Decode, so (like the in-memory backend) it covers `extract` only
// (returns `i64`); `now()`/`date_trunc` need a decodable bare timestamp. `date_trunc` is also
// PostgreSQL-only (no `SupportsDateTrunc` for MySQL), so using it here would be a compile error.
#[cfg(feature = "systemtime")]
#[derive(Clone, Debug, PartialEq, Table)]
struct TimedEvent<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    created: C::Type<'scope, std::time::SystemTime>,
}

#[cfg(feature = "systemtime")]
#[test]
fn mysql_now_renders_with_microsecond_precision() {
    // MySQL's bare `CURRENT_TIMESTAMP` is fsp 0; `now()` renders `CURRENT_TIMESTAMP(6)` so the
    // microsecond value survives into a `TIMESTAMP(6)` column.
    let q = Mysql
        .from::<TimedEvent>()
        .where_(|e| e.created.equals(now::<std::time::SystemTime>()))
        .select(|(e,)| e.id);
    assert!(
        q.to_sql().contains("CURRENT_TIMESTAMP(6)"),
        "{}",
        q.to_sql()
    );
    assert!(!q.to_sql().contains("CURRENT_TIMESTAMP)"), "{}", q.to_sql());
}

#[cfg(feature = "systemtime")]
#[test]
fn mysql_extract_renders() {
    // `extract` -> EXTRACT wrapped in a CAST to SIGNED (MySQL's CAST integer target).
    let q = Mysql
        .from::<TimedEvent>()
        .select(|(e,)| extract(DateField::Year, e.created));
    assert!(
        q.to_sql()
            .contains("CAST(EXTRACT(YEAR FROM q0_0.`created`) AS SIGNED)"),
        "{}",
        q.to_sql()
    );

    // `extract(Second)` -> floored whole seconds (FLOOR is a no-op on MySQL's integer SECOND).
    let second_q = Mysql
        .from::<TimedEvent>()
        .select(|(e,)| extract(DateField::Second, e.created));
    assert!(
        second_q
            .to_sql()
            .contains("CAST(FLOOR(EXTRACT(SECOND FROM q0_0.`created`)) AS SIGNED)"),
        "{}",
        second_q.to_sql()
    );

    // `extract_second` -> fractional seconds via the composite SECOND_MICROSECOND unit, cast to DOUBLE.
    let frac_q = Mysql
        .from::<TimedEvent>()
        .select(|(e,)| extract_second(e.created));
    assert!(
        frac_q.to_sql().contains(
            "CAST(EXTRACT(SECOND_MICROSECOND FROM q0_0.`created`) / 1000000.0 AS DOUBLE)"
        ),
        "{}",
        frac_q.to_sql()
    );
}

#[test]
fn mysql_cross_join_renders_cross_join() {
    let q = Mysql
        .from::<Widget>()
        .cross_join::<Gadget>()
        .select(|(widget, gadget)| (widget.id, gadget.id));
    let sql = q.to_sql();
    assert!(
        sql.contains("FROM `widgets` AS q0_0 CROSS JOIN `gadgets` AS q0_1"),
        "{sql}"
    );
    assert!(!sql.contains(" ON "), "{sql}");
}

#[test]
fn mysql_self_join_renders_distinct_aliases() {
    let q = Mysql
        .from::<Gadget>()
        .join::<Gadget>()
        .on(|(gadget,), other| gadget.widget_id.equals(other.id))
        .select(|(gadget, other)| (gadget.id, other.id));
    let sql = q.to_sql();
    assert!(
        sql.contains(
            "FROM `gadgets` AS q0_0 INNER JOIN `gadgets` AS q0_1 ON (q0_0.`widget_id` = q0_1.`id`)"
        ),
        "{sql}"
    );
}
