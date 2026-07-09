//! PostgreSQL rendering of a recursive CTE: `WITH RECURSIVE "name" ("cols") AS (anchor UNION ALL
//! recursive)`, `"`-quoted identifiers, self-reference as the bare CTE name.

use squealy::*;
use squealy_postgresql::Postgres;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Node<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    parent_id: C::Type<'scope, Option<i32>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    nodes: Node<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, RecursiveCTE)]
struct Ancestor<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    depth: C::Type<'scope, i32>,
}

impl<'scope, C: ColumnMode> RecursiveCteDefinition for Ancestor<'scope, C> {
    const UNION_ALL: bool = true;

    fn definition(
        db: &'static ModelConn,
        recur: RecursiveSelf<'static, Self>,
    ) -> impl RecursiveBody<Row = <Self as SchemaCte>::Row> {
        let anchor = db
            .from::<Node>()
            .where_(|node| node.parent_id.is_null())
            .project(|(node,)| (node.id, 0));
        let step = recur
            .from()
            .join::<Node>()
            .on(|(ancestor,), node| node.parent_id.equals(ancestor.id))
            .project(|(ancestor, node)| (node.id, ancestor.depth + 1));
        anchor.union_with(step)
    }
}

// A recursive CTE whose anchor carries its own ORDER BY/LIMIT — a scoped arm PostgreSQL renders
// parenthesized (see `Dialect::supports_parenthesized_recursive_cte_arm`).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, RecursiveCTE)]
struct BoundedAncestor<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    depth: C::Type<'scope, i32>,
}

impl<'scope, C: ColumnMode> RecursiveCteDefinition for BoundedAncestor<'scope, C> {
    const UNION_ALL: bool = true;

    fn definition(
        db: &'static ModelConn,
        recur: RecursiveSelf<'static, Self>,
    ) -> impl RecursiveBody<Row = <Self as SchemaCte>::Row> {
        let anchor = db
            .from::<Node>()
            .where_(|node| node.parent_id.is_null())
            .order_by(|(node,)| node.id.asc())
            .limit(5)
            .project(|(node,)| (node.id, 0));
        let step = recur
            .from()
            .join::<Node>()
            .on(|(ancestor,), node| node.parent_id.equals(ancestor.id))
            .project(|(ancestor, node)| (node.id, ancestor.depth + 1));
        anchor.union_with(step)
    }
}

#[test]
fn postgres_scoped_recursive_cte_arm_renders_parenthesized() {
    // The scoped anchor (its own ORDER BY/LIMIT) renders parenthesized; the unscoped recursive arm bare.
    let query = Postgres
        .from::<BoundedAncestor>()
        .select(|(a,)| (a.id, a.depth));
    assert_eq!(
        query.to_sql(),
        "WITH RECURSIVE \"bounded_ancestors\" (\"id\", \"depth\") AS (\
(SELECT q0_0.\"id\" AS \"t0_id\", 0 AS \"t1_expr\" FROM \"public\".\"nodes\" AS q0_0 \
WHERE (q0_0.\"parent_id\" IS NULL) ORDER BY q0_0.\"id\" ASC LIMIT 5) \
UNION ALL \
SELECT q0_1.\"id\" AS \"t0_id\", (q0_0.\"depth\" + 1) AS \"t1_expr\" FROM \"bounded_ancestors\" AS q0_0 \
INNER JOIN \"public\".\"nodes\" AS q0_1 ON (q0_1.\"parent_id\" = q0_0.\"id\")) \
SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"depth\" AS \"t1_depth\" FROM \"bounded_ancestors\" AS q0_0"
    );
}

#[test]
fn postgres_recursive_cte() {
    let query = Postgres.from::<Ancestor>().select(|(a,)| (a.id, a.depth));

    assert_eq!(
        query.to_sql(),
        "WITH RECURSIVE \"ancestors\" (\"id\", \"depth\") AS (\
SELECT q0_0.\"id\" AS \"t0_id\", 0 AS \"t1_expr\" FROM \"public\".\"nodes\" AS q0_0 \
WHERE (q0_0.\"parent_id\" IS NULL) \
UNION ALL \
SELECT q0_1.\"id\" AS \"t0_id\", (q0_0.\"depth\" + 1) AS \"t1_expr\" FROM \"ancestors\" AS q0_0 \
INNER JOIN \"public\".\"nodes\" AS q0_1 ON (q0_1.\"parent_id\" = q0_0.\"id\")) \
SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"depth\" AS \"t1_depth\" FROM \"ancestors\" AS q0_0"
    );
}
