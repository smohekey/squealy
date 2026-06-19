use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // A `HAVING` with no `GROUP BY` is a whole-table aggregate, so its predicate may only reference
    // aggregates — `HAVING (id > ?)` on a bare column is invalid SQL. (Adding `group_by(|(u,)| u.id)`
    // would make `id` a grouping key and allow it.)
    let _query = TestConnection
        .from::<User>()
        .having(|(user,)| user.id.greater_than(0))
        .select(|(user,)| user.id.count());
}
