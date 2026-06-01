use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    count: C::Type<'scope, i32>,
}

fn main() {
    let counter = <Counter as ProjectionShape>::exprs(SourceAlias::new(0, 0));
    let _insert = TestConnection
        .to_columns::<Counter, (CounterCount,)>()
        .row((counter.count + 1,))
        .insert();
}
