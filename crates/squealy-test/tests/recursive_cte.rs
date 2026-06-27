//! Recursive CTEs (`WITH RECURSIVE`) on the test backend: a `#[derive(RecursiveCTE)]` whose body is
//! `<anchor> UNION [ALL] <recursive>`, where the recursive term self-references via `recur.from()`.

use squealy::*;
use squealy_test::TestConnection;

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

// A plain (non-recursive) CTE used alongside the recursive one: the whole WITH becomes RECURSIVE.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, CTE)]
struct RootNode<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    parent_id: C::Type<'scope, Option<i32>>,
}

impl<'scope, C: ColumnMode> CteDefinition for RootNode<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaCte>::Row> {
        db.from::<Node>()
            .where_(|node| node.parent_id.is_null())
            .project(|(node,)| (node.id, node.parent_id))
    }
}

#[test]
fn recursive_and_plain_cte_share_one_with_recursive() {
    let query = TestConnection
        .from::<Ancestor>()
        .join::<RootNode>()
        .on(|(ancestor,), root| root.id.equals(ancestor.id))
        .select(|(ancestor, _root)| (ancestor.id, ancestor.depth));

    let sql = query.to_sql();
    assert!(
        sql.starts_with("WITH RECURSIVE "),
        "single RECURSIVE prefix: {sql}"
    );
    // Both CTE definitions appear once, with `ancestors` (the recursive one) using UNION ALL.
    assert!(
        sql.contains("ancestors (id, depth) AS ("),
        "missing ancestors: {sql}"
    );
    assert!(
        sql.contains("root_nodes (id, parent_id) AS ("),
        "missing root_nodes: {sql}"
    );
    assert_eq!(sql.matches("WITH RECURSIVE").count(), 1);
}

// A *different* CTE that derives the same bare name as the recursive one must NOT be filtered out as if
// it were the self-reference (the self-edge is matched by definition identity, not name): it is kept,
// and the collector reports the name collision. `a::Dup` (recursive) and `b::Dup` both derive "dups".
mod collision {
    use super::*;

    pub mod b {
        use super::*;
        #[allow(dead_code)]
        #[derive(Clone, Debug, PartialEq, CTE)]
        pub struct Dup<'scope, C: ColumnMode = ColumnExpr> {
            pub id: C::Type<'scope, i32>,
            pub depth: C::Type<'scope, i32>,
        }
        impl<'scope, C: ColumnMode> CteDefinition for Dup<'scope, C> {
            fn definition(
                db: &'static ModelConn,
            ) -> impl ViewSelect<Row = <Self as SchemaCte>::Row> {
                db.from::<Node>().project(|(node,)| (node.id, node.id))
            }
        }
    }

    pub mod a {
        use super::*;
        #[allow(dead_code)]
        #[derive(Clone, Debug, PartialEq, RecursiveCTE)]
        pub struct Dup<'scope, C: ColumnMode = ColumnExpr> {
            pub id: C::Type<'scope, i32>,
            pub depth: C::Type<'scope, i32>,
        }
        impl<'scope, C: ColumnMode> RecursiveCteDefinition for Dup<'scope, C> {
            fn definition(
                db: &'static ModelConn,
                recur: RecursiveSelf<'static, Self>,
            ) -> impl RecursiveBody<Row = <Self as SchemaCte>::Row> {
                // The anchor references the *other* "dups" CTE; the recursive arm references self.
                let anchor = db.from::<super::b::Dup>().project(|(d,)| (d.id, d.depth));
                let step = recur.from().project(|(d,)| (d.id, d.depth));
                anchor.union_with(step)
            }
        }
    }
}

#[test]
#[should_panic(expected = "colliding CTE names")]
fn different_cte_sharing_recursive_name_is_a_collision() {
    let query = TestConnection
        .from::<collision::a::Dup>()
        .select(|(d,)| (d.id, d.depth));
    let _ = query.to_sql();
}

#[test]
fn recursive_cte_renders_with_recursive_union_all() {
    let query = TestConnection
        .from::<Ancestor>()
        .select(|(a,)| (a.id, a.depth));

    assert_eq!(
        query.to_sql(),
        "WITH RECURSIVE ancestors (id, depth) AS (\
(SELECT q0_0.id, 0 FROM public.nodes AS q0_0 WHERE (q0_0.parent_id IS NULL)) \
UNION ALL \
(SELECT q0_1.id, (q0_0.depth + 1) FROM ancestors AS q0_0 \
INNER JOIN public.nodes AS q0_1 ON (q0_1.parent_id = q0_0.id))) \
SELECT q0_0.id AS t0_id, q0_0.depth AS t1_depth FROM ancestors AS q0_0"
    );
}
