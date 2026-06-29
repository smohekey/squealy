use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    count: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // A correlated `DELETE … USING` must carry a join predicate. Without `.where_(...)` the builder is
    // still in the unfiltered state, so `.build()`/`.delete()` are unavailable — otherwise it would
    // render `DELETE … JOIN other ON` with an empty `ON`, which the database rejects.
    let conn = TestConnection;
    let _query = conn.from::<Counter>().using::<Post>().build();
}
