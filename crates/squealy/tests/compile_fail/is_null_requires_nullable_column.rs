use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // `name` is NOT NULL, so `is_null()` must not compile: the test would be a constant.
    let _query = TestConnection
        .from::<Account>()
        .where_(|account| account.name.is_null())
        .select(|(account,)| account.id);
}