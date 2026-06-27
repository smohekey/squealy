use squealy::*;

// A recursive CTE's anchor and recursive arms must produce the same row type (and match the declared
// columns). An anchor of `(i32, i32)` with a recursive arm of `(i32, String)` is rejected.

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Node<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
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
    fn definition(
        db: &'static ModelConn,
        recur: RecursiveSelf<'static, Self>,
    ) -> impl RecursiveBody<Row = <Self as SchemaCte>::Row> {
        let anchor = db
            .from::<Node>()
            .where_(|node| node.parent_id.is_null())
            .project(|(node,)| (node.id, 0));
        // Wrong: second column is `String`, not the anchor's `i32`.
        let step = recur
            .from()
            .join::<Node>()
            .on(|(ancestor,), node| node.parent_id.equals(ancestor.id))
            .project(|(_ancestor, node)| (node.id, node.name));
        anchor.union_with(step)
    }
}

fn main() {}
