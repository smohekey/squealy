use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Item<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // A SELECT list mixing a bare column with an aggregate is invalid without GROUP BY, so the
    // mixed tuple has no `ProjectionClass` impl and `select` rejects it.
    let _query = TestConnection
        .from::<Item>()
        .select(|(item,)| (item.id, item.id.count()));
}
