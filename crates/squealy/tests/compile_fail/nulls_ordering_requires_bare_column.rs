use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // `NULLS FIRST/LAST` is only offered on a bare-column order term — a derived expression's
    // null placement is out of scope (its MySQL `(<expr> IS NULL)` emulation cannot stay valid under
    // `SELECT DISTINCT`). Ordering by `id + 1` with `.nulls_last()` therefore does not compile.
    let _query = TestConnection
        .from::<User>()
        .order_by(|(user,)| (user.id + 1).asc().nulls_last())
        .select(|(user,)| user.id);
}
