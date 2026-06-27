//! PostgreSQL rendering of non-recursive CTEs: referencing a `#[derive(CTE)]` type emits its body as
//! an automatic `WITH "name" ("cols") AS (…)` prefix, with PostgreSQL `"`-quoted identifiers.

use squealy::*;
use squealy_postgresql::Postgres;

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
fn postgres_single_cte_in_from() {
    let query = Postgres
        .from::<ActiveUser>()
        .select(|(active,)| (active.id, active.name));

    assert_eq!(
        query.to_sql(),
        "WITH \"active_users\" (\"id\", \"name\") AS (\
SELECT q0_0.\"id\", q0_0.\"name\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"active\" = TRUE)) \
SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"name\" AS \"t1_name\" FROM \"active_users\" AS q0_0"
    );
}

#[test]
fn postgres_multiple_ctes_across_from_and_join() {
    let query = Postgres
        .from::<ActiveUser>()
        .join::<BigOrder>()
        .on(|(active,), order| order.user_id.equals(active.id))
        .select(|(active, order)| (active.name, order.total));

    assert_eq!(
        query.to_sql(),
        "WITH \"active_users\" (\"id\", \"name\") AS (\
SELECT q0_0.\"id\", q0_0.\"name\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"active\" = TRUE)), \
\"big_orders\" (\"user_id\", \"total\") AS (\
SELECT q0_0.\"user_id\", q0_0.\"total\" FROM \"public\".\"orders\" AS q0_0 WHERE (q0_0.\"total\" > 100)) \
SELECT q0_0.\"name\" AS \"t0_name\", q0_1.\"total\" AS \"t1_total\" \
FROM \"active_users\" AS q0_0 INNER JOIN \"big_orders\" AS q0_1 ON (q0_1.\"user_id\" = q0_0.\"id\")"
    );
}
