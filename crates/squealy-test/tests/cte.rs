//! End-to-end rendering of non-recursive CTEs: referencing a `#[derive(CTE)]` type with
//! `from`/joins emits its body as an automatic `WITH` prefix, bound by the CTE's bare name. Covers a
//! single CTE, multiple CTEs across `FROM`+`JOIN`, and a CTE referenced twice (one `WITH` entry).

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
struct Order<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
    total: C::Type<'scope, i64>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
    orders: Order<'static, ColumnName>,
}

// A CTE selecting the active users.
#[allow(dead_code)]
#[derive(CTE)]
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

// A second, independent CTE (over a different base table) for the multi-CTE cases.
#[allow(dead_code)]
#[derive(CTE)]
struct BigOrder<'scope, C: ColumnMode = ColumnExpr> {
    user_id: C::Type<'scope, i32>,
    total: C::Type<'scope, i64>,
}

impl<'scope, C: ColumnMode> CteDefinition for BigOrder<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaCte>::Row> {
        db.from::<Order>()
            .where_(|order| order.total.greater_than(100i64))
            .project(|(order,)| (order.user_id, order.total))
    }
}

#[test]
fn single_cte_in_from_emits_with_prefix() {
    let query = TestConnection
        .from::<ActiveUser>()
        .select(|(active,)| (active.id, active.name));

    assert_eq!(
        query.to_sql(),
        "WITH active_users (id, name) AS (\
SELECT q0_0.id, q0_0.name FROM public.users AS q0_0 WHERE (q0_0.active = TRUE)) \
SELECT q0_0.id AS t0_id, q0_0.name AS t1_name FROM active_users AS q0_0"
    );
    // A CTE body is parameter-free (literals only), so the query still binds no params.
    assert_eq!(query.collect_params().unwrap(), Vec::<TestParam>::new());
}

#[test]
fn multiple_ctes_across_from_and_join() {
    let query = TestConnection
        .from::<ActiveUser>()
        .join::<BigOrder>()
        .on(|(active,), order| order.user_id.equals(active.id))
        .select(|(active, order)| (active.name, order.total));

    assert_eq!(
        query.to_sql(),
        "WITH active_users (id, name) AS (\
SELECT q0_0.id, q0_0.name FROM public.users AS q0_0 WHERE (q0_0.active = TRUE)), \
big_orders (user_id, total) AS (\
SELECT q0_0.user_id, q0_0.total FROM public.orders AS q0_0 WHERE (q0_0.total > 100)) \
SELECT q0_0.name AS t0_name, q0_1.total AS t1_total \
FROM active_users AS q0_0 INNER JOIN big_orders AS q0_1 ON (q0_1.user_id = q0_0.id)"
    );
}

#[test]
fn cte_referenced_twice_emits_one_with_entry() {
    // A self-join over the same CTE references its `CteDef` twice; the `WITH` set is de-duplicated to
    // a single entry while both `FROM`/`JOIN` aliases still bind the CTE name.
    let query = TestConnection
        .from::<ActiveUser>()
        .join::<ActiveUser>()
        .on(|(first,), second| second.id.equals(first.id))
        .select(|(first, second)| (first.id, second.name));

    assert_eq!(
        query.to_sql(),
        "WITH active_users (id, name) AS (\
SELECT q0_0.id, q0_0.name FROM public.users AS q0_0 WHERE (q0_0.active = TRUE)) \
SELECT q0_0.id AS t0_id, q0_1.name AS t1_name \
FROM active_users AS q0_0 INNER JOIN active_users AS q0_1 ON (q0_1.id = q0_0.id)"
    );
}

// Two *distinct* CTE types that derive the same bare name (`#[derive(CTE)]` names by struct name,
// ignoring the module) would both bind a single `WITH` entry, silently dropping one body. That is
// rejected rather than rendered as wrong SQL.
mod collision {
    use super::*;

    pub mod left {
        use super::*;

        #[allow(dead_code)]
        #[derive(CTE)]
        pub struct Summary<'scope, C: ColumnMode = ColumnExpr> {
            pub id: C::Type<'scope, i32>,
            pub name: C::Type<'scope, String>,
        }

        impl<'scope, C: ColumnMode> CteDefinition for Summary<'scope, C> {
            fn definition(
                db: &'static ModelConn,
            ) -> impl ViewSelect<Row = <Self as SchemaCte>::Row> {
                db.from::<User>()
                    .where_(|user| user.active.equals(true))
                    .project(|(user,)| (user.id, user.name))
            }
        }
    }

    pub mod right {
        use super::*;

        #[allow(dead_code)]
        #[derive(CTE)]
        pub struct Summary<'scope, C: ColumnMode = ColumnExpr> {
            pub id: C::Type<'scope, i32>,
            pub name: C::Type<'scope, String>,
        }

        impl<'scope, C: ColumnMode> CteDefinition for Summary<'scope, C> {
            fn definition(
                db: &'static ModelConn,
            ) -> impl ViewSelect<Row = <Self as SchemaCte>::Row> {
                db.from::<User>()
                    .where_(|user| user.active.equals(false))
                    .project(|(user,)| (user.id, user.name))
            }
        }
    }
}

#[test]
#[should_panic(expected = "two distinct CTEs are both named")]
fn distinct_ctes_with_colliding_names_are_rejected() {
    let query = TestConnection
        .from::<collision::left::Summary>()
        .join::<collision::right::Summary>()
        .on(|(left,), right| right.id.equals(left.id))
        .select(|(left, right)| (left.name, right.name));

    // Both CTEs derive the bare name "summaries"; rendering must fail loudly rather than emit one body.
    let _ = query.to_sql();
}
