//! PostgreSQL rendering of set operations: `"`-quoted identifiers, `$n` placeholders, continuous
//! placeholder numbering across arms, nesting, trailing ORDER BY/LIMIT, and CTE hoisting.

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

#[test]
fn postgres_union_and_intersect_all() {
    let union = Postgres
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .union(Postgres.from::<Admin>().select(|(a,)| (a.id, a.name)));

    assert_eq!(
        union.to_sql(),
        "(SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"name\" AS \"t1_name\" FROM \"public\".\"users\" AS q0_0) \
UNION \
(SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"name\" AS \"t1_name\" FROM \"public\".\"admins\" AS q0_0)"
    );

    let intersect_all = Postgres
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .intersect_all(Postgres.from::<Admin>().select(|(a,)| (a.id, a.name)));
    assert!(intersect_all.to_sql().contains(") INTERSECT ALL ("));
}

#[test]
fn postgres_continuous_placeholders_across_arms() {
    let query = Postgres
        .from::<User>()
        .where_(|u| u.id.equals(1))
        .select(|(u,)| (u.id, u.name))
        .union(
            Postgres
                .from::<Admin>()
                .where_(|a| a.id.equals(2))
                .select(|(a,)| (a.id, a.name)),
        );

    // Placeholders are numbered continuously across both arms ($1 then $2).
    assert_eq!(
        query.to_sql(),
        "(SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"name\" AS \"t1_name\" FROM \"public\".\"users\" AS q0_0 \
WHERE (q0_0.\"id\" = $1)) \
UNION \
(SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"name\" AS \"t1_name\" FROM \"public\".\"admins\" AS q0_0 \
WHERE (q0_0.\"id\" = $2))"
    );
}

#[test]
fn postgres_nesting_and_trailing_clause() {
    let query = Postgres
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .union(Postgres.from::<Admin>().select(|(a,)| (a.id, a.name)))
        .intersect(Postgres.from::<User>().select(|(u,)| (u.id, u.name)))
        .order_by_asc(|out| out.0)
        .limit(10);

    let sql = query.to_sql();
    assert!(
        sql.starts_with("(("),
        "outer set should parenthesize the nested union: {sql}"
    );
    assert!(
        sql.contains(")) INTERSECT ("),
        "nested union grouped before intersect: {sql}"
    );
    assert!(
        sql.ends_with(") ORDER BY \"t0_id\" ASC LIMIT 10"),
        "trailing order/limit: {sql}"
    );
}
