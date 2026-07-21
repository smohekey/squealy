//! MySQL rendering of a recursive CTE: `WITH RECURSIVE `name` (`cols`) AS (anchor UNION ALL
//! recursive)`, backtick-quoted identifiers, self-reference as the bare CTE name.

use squealy::*;
use squealy_mysql::Mysql;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
struct Node<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	parent_id: C::Type<'scope, Option<i32>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Shop {
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

#[test]
fn mysql_recursive_cte() {
	let query = Mysql.from::<Ancestor>().select(|(a,)| (a.id, a.depth));

	assert_eq!(
		query.to_sql(),
		"WITH RECURSIVE `ancestors` (`id`, `depth`) AS (\
SELECT q0_0.`id` AS `t0_id`, 0 AS `t1_expr` FROM `shop`.`nodes` AS q0_0 \
WHERE (q0_0.`parent_id` IS NULL) \
UNION ALL \
SELECT q0_1.`id` AS `t0_id`, (q0_0.`depth` + 1) AS `t1_expr` FROM `ancestors` AS q0_0 \
INNER JOIN `shop`.`nodes` AS q0_1 ON (q0_1.`parent_id` = q0_0.`id`)) \
SELECT q0_0.`id` AS `t0_id`, q0_0.`depth` AS `t1_depth` FROM `ancestors` AS q0_0"
	);
}

// A recursive CTE whose anchor carries its own ORDER BY/LIMIT — a *scoped* arm. MySQL renders it
// parenthesized `(anchor … LIMIT n) UNION ALL …` (only SQLite forbids that), so the render succeeds.
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
fn mysql_scoped_recursive_arm_renders_parenthesized() {
	// The shape SQLite rejects renders fine on MySQL — the reject is dialect-specific, so `to_sql()`
	// must not panic and `try_to_sql()` must return `Ok` with the parenthesized arm.
	let query = Mysql
		.from::<BoundedAncestor>()
		.select(|(ancestor,)| (ancestor.id, ancestor.depth));
	let sql = query
		.try_to_sql()
		.expect("MySQL renders a scoped recursive arm");
	assert_eq!(sql, query.to_sql());
	assert!(
		sql.contains("(SELECT ") && sql.contains("LIMIT 5) UNION ALL "),
		"expected a parenthesized, limited anchor arm: {sql}"
	);
}
