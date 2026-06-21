use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // `DISTINCT` is invalid with `OVER (…)`, so `.over()` is unavailable once `.distinct()` has been
    // applied: the window builder only exists on a non-distinct aggregate.
    let _query = TestConnection
        .from::<Post>()
        .select(|(post,)| post.id.count().distinct().over(|w| w.order_by(post.id.asc())));
}
