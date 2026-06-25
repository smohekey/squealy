use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Flag<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    active: C::Type<'scope, bool>,
}

fn main() {
    // PostgreSQL has no `min`/`max` aggregate for boolean values, so `bool` is excluded from
    // `AggregateScalar` and `active.min()` must not compile.
    let _query = TestConnection
        .from::<Flag>()
        .select(|(flag,)| flag.active.min());
}
