use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    maybe_total: C::Type<'scope, Option<i64>>,
}

fn main() {
    // Aggregates are invalid in `UPDATE ... SET`, including for a nullable target column (which
    // takes the `IntoNullableAssignmentValue` path), so the value must be aggregate-free.
    let _query = TestConnection
        .to_columns::<Counter, (CounterMaybeTotal,)>()
        .set(|counter| (counter.maybe_total.count(),));
}
