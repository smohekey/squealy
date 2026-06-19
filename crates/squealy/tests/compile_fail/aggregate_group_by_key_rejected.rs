use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // A grouping item may not contain an aggregate — `GROUP BY COUNT(id)` is invalid SQL, so
    // `GroupByKeys` only accepts a `NonAggregateAst` key.
    let _query = TestConnection
        .from::<User>()
        .group_by(|(user,)| user.id.count())
        .select(|(user,)| user.id.count());
}
