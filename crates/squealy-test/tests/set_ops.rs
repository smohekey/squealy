//! Set operations (UNION / INTERSECT / EXCEPT, incl. ALL variants), nesting, and trailing
//! ORDER BY / LIMIT / OFFSET on the test backend.

use squealy::*;
use squealy_test::{TestConnection, TestParam};

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    active: C::Type<'scope, bool>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Admin<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
    admins: Admin<'static, ColumnName>,
}

// A CTE, to check that CTEs referenced inside a set-op arm are hoisted into one leading WITH.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, CTE)]
struct ActiveUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> CteDefinition for ActiveUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaCte>::Row> {
        db.from::<User>()
            .where_(|user| user.active.equals(true))
            .project(|(user,)| (user.id, user.name))
    }
}

#[test]
fn cte_in_arm_is_hoisted_to_one_leading_with() {
    let query = TestConnection
        .from::<ActiveUser>()
        .select(|(a,)| (a.id, a.name))
        .union(TestConnection.from::<Admin>().select(|(x,)| (x.id, x.name)));

    assert_eq!(
        query.to_sql(),
        "WITH active_users (id, name) AS (\
SELECT q0_0.id, q0_0.name FROM public.users AS q0_0 WHERE (q0_0.active = TRUE)) \
(SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM active_users AS q0_0) \
UNION \
(SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM public.admins AS q0_0)"
    );
}

#[test]
fn union_of_two_selects() {
    let query = TestConnection
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .union(TestConnection.from::<Admin>().select(|(a,)| (a.id, a.name)));

    assert_eq!(
        query.to_sql(),
        "(SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM public.users AS q0_0) \
UNION \
(SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM public.admins AS q0_0)"
    );
}

#[test]
fn all_six_operators_render() {
    // Each operator renders between the two parenthesized arms.
    macro_rules! assert_op {
        ($method:ident, $keyword:literal) => {{
            let sql = TestConnection
                .from::<User>()
                .select(|(u,)| (u.id, u.name))
                .$method(TestConnection.from::<Admin>().select(|(a,)| (a.id, a.name)))
                .to_sql();
            assert!(
                sql.contains(concat!(") ", $keyword, " (")),
                "missing {}: {sql}",
                $keyword
            );
        }};
    }
    assert_op!(union, "UNION");
    assert_op!(union_all, "UNION ALL");
    assert_op!(intersect, "INTERSECT");
    assert_op!(intersect_all, "INTERSECT ALL");
    assert_op!(except, "EXCEPT");
    assert_op!(except_all, "EXCEPT ALL");
}

#[test]
fn nesting_parenthesizes() {
    let query = TestConnection
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .union(TestConnection.from::<Admin>().select(|(a,)| (a.id, a.name)))
        .intersect(TestConnection.from::<User>().select(|(u,)| (u.id, u.name)));

    assert_eq!(
        query.to_sql(),
        "((SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM public.users AS q0_0) \
UNION \
(SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM public.admins AS q0_0)) \
INTERSECT \
(SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM public.users AS q0_0)"
    );
}

#[test]
fn trailing_order_by_and_limit() {
    let query = TestConnection
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .union(TestConnection.from::<Admin>().select(|(a,)| (a.id, a.name)))
        .order_by_desc(|out| out.0)
        .limit(10)
        .offset(5);

    assert_eq!(
        query.to_sql(),
        "(SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM public.users AS q0_0) \
UNION \
(SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM public.admins AS q0_0) \
ORDER BY t0_id DESC LIMIT 10 OFFSET 5"
    );
}

#[test]
fn param_order_across_arms() {
    let query = TestConnection
        .from::<User>()
        .where_(|u| u.id.equals(1))
        .select(|(u,)| (u.id, u.name))
        .union(
            TestConnection
                .from::<Admin>()
                .where_(|a| a.id.equals(2))
                .select(|(a,)| (a.id, a.name)),
        );

    assert_eq!(
        query.collect_params().unwrap(),
        vec![TestParam::Int(1), TestParam::Int(2)]
    );
}
