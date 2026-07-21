use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Job<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // A row lock requires a select that identifies individual rows — combining `FOR UPDATE` with
    // `DISTINCT` is rejected by the database, so it is a compile error here (in either chain order).
    let _distinct_then_lock = TestConnection
        .from::<Job>()
        .distinct()
        .for_update()
        .select(|(job,)| job.id);

    let _lock_then_distinct = TestConnection
        .from::<Job>()
        .for_update()
        .distinct()
        .select(|(job,)| job.id);
}
