use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Item<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // Aggregates are invalid in a WHERE clause (PostgreSQL/MySQL require them in the select list or
    // HAVING), so a predicate built from an aggregate must not satisfy `where_`'s
    // `NonAggregatePredicate` bound.
    let _query = TestConnection
        .from::<Item>()
        .where_(|item| item.id.count().greater_than(0i64))
        .select(|(item,)| item.id);
}
