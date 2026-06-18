use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // Ordering an aggregate-only query by a base (ungrouped) column is invalid without GROUP BY.
    let _query = TestConnection
        .from::<User>()
        .order_by(|(user,)| user.name.asc())
        .select(|(user,)| user.id.count());
}
