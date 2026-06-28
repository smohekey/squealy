use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Job<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // `FOR UPDATE` with an aggregate projection does not identify individual rows, so the database
    // rejects it — caught here at compile time (the locked select must be scalar).
    let _query = TestConnection
        .from::<Job>()
        .for_update()
        .select(|(job,)| job.id.count());
}
