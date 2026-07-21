use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // A `HAVING` with no `GROUP BY` evaluates the table as a single group, so a bare-column
    // projection is invalid SQL — the chain is `Aggregated`, which requires an aggregate-only
    // projection. (Adding a `group_by(|(u,)| u.id)` would make this valid.)
    let _query = TestConnection
        .from::<User>()
        .having(|(user,)| user.id.count().greater_than(0i64))
        .select(|(user,)| user.id);
}
