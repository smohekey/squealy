use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Item<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // Aggregates are never valid in a RETURNING clause, so the returning projection must be
    // aggregate-free (`ProjectionClass<Class = ScalarProjection>`).
    let _query = TestConnection
        .to::<Item>()
        .name("x")
        .insert_returning(|item| item.id.count());
}
