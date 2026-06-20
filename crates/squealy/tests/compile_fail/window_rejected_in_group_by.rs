use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // Window functions are computed after WHERE/GROUP BY, so a window expression is not a
    // `NonAggregateAst` and cannot be used as a `GROUP BY` key.
    let _query = TestConnection
        .from::<Post>()
        .group_by(|(post,)| row_number().over(|w| w.order_by(post.id.asc())))
        .select(|(post,)| post.user_id);
}
