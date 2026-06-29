use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    email: C::Type<'scope, String>,
}

fn main() {
    // `name` and `email` are both required (non-null, no default). The target columns must cover every
    // required column; omitting `email` is a compile error (it would fail at execution otherwise).
    let conn = TestConnection;
    let _query = conn.to::<Account>().insert_select(
        |account| account.name,
        conn.from::<Account>().select(|(account,)| account.name),
    );
}
