use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    // `i64` so it can be added to `COUNT(...)` (also `i64`).
    big: C::Type<'scope, i64>,
}

fn main() {
    // `COUNT(id) + big` mixes an aggregate with a bare (ungrouped) column, which is invalid without
    // GROUP BY — `CombineTerm` has no impl for column + aggregate, so it has no `ProjectionClass`.
    let _query = TestConnection
        .from::<User>()
        .select(|(user,)| user.id.count() + user.big);
}
