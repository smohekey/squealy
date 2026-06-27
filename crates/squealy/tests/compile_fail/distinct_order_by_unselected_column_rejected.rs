use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // SELECT DISTINCT requires every ORDER BY key to appear in the projection: ordering by `id` while
    // projecting only `name` is rejected at compile time.
    let _query = TestConnection
        .from::<User>()
        .distinct()
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user.name);
}
