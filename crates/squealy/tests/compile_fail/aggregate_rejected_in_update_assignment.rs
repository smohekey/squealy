use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    // `i64` so `COUNT(...)` (which yields `i64`) type-checks as a value and the failure is the
    // aggregate guard, not a value-type mismatch.
    total: C::Type<'scope, i64>,
}

fn main() {
    // An aggregate is invalid as an `UPDATE ... SET` value, so the assignment expression must be
    // aggregate-free (`Ast: NonAggregateAst`).
    let _query = TestConnection
        .to_columns::<Counter, (CounterTotal,)>()
        .set(|counter| (counter.total.count(),));
}
