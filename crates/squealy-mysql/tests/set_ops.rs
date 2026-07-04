//! MySQL rendering of set operations: backtick-quoted identifiers, `?` placeholders, nesting, and a
//! trailing ORDER BY / LIMIT on the whole set.

use squealy::*;
use squealy_mysql::Mysql;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    active: C::Type<'scope, bool>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
struct Admin<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Shop {
    users: User<'static, ColumnName>,
    admins: Admin<'static, ColumnName>,
}

#[test]
fn mysql_union_all() {
    let query = Mysql
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .union_all(Mysql.from::<Admin>().select(|(a,)| (a.id, a.name)));

    assert_eq!(
        query.to_sql(),
        "(SELECT q0_0.`id` AS `t0_id`, q0_0.`name` AS `t1_name` FROM `shop`.`users` AS q0_0) \
UNION ALL \
(SELECT q0_0.`id` AS `t0_id`, q0_0.`name` AS `t1_name` FROM `shop`.`admins` AS q0_0)"
    );
}

#[test]
fn mysql_intersect_and_intersect_all() {
    let intersect = Mysql
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .intersect(Mysql.from::<Admin>().select(|(a,)| (a.id, a.name)));

    assert_eq!(
        intersect.to_sql(),
        "(SELECT q0_0.`id` AS `t0_id`, q0_0.`name` AS `t1_name` FROM `shop`.`users` AS q0_0) \
INTERSECT \
(SELECT q0_0.`id` AS `t0_id`, q0_0.`name` AS `t1_name` FROM `shop`.`admins` AS q0_0)"
    );

    let intersect_all = Mysql
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .intersect_all(Mysql.from::<Admin>().select(|(a,)| (a.id, a.name)));
    assert!(intersect_all.to_sql().contains(") INTERSECT ALL ("));
}

#[test]
fn mysql_except_with_trailing_order_limit() {
    let query = Mysql
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .except(Mysql.from::<Admin>().select(|(a,)| (a.id, a.name)))
        .order_by_desc(|out| out.1)
        .limit(5);

    assert_eq!(
        query.to_sql(),
        "(SELECT q0_0.`id` AS `t0_id`, q0_0.`name` AS `t1_name` FROM `shop`.`users` AS q0_0) \
EXCEPT \
(SELECT q0_0.`id` AS `t0_id`, q0_0.`name` AS `t1_name` FROM `shop`.`admins` AS q0_0) \
ORDER BY `t1_name` DESC LIMIT 5"
    );
}
