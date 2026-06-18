use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Item<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // SQL aggregate arguments cannot contain another aggregate at the same query level
    // (`SUM(SUM(...))` is rejected), so an aggregate result is not a valid operand for another
    // aggregate: the operand must be `NonAggregateAst`.
    let _query = TestConnection
        .from::<Item>()
        .select(|(item,)| item.id.sum().sum());
}
