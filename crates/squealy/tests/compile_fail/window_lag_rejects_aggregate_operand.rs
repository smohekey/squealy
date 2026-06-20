use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // LAG/LEAD operands must be row-level scalars: an aggregate (or nested window) operand is
    // rejected by the backends, so `lag` requires `NonAggregateAst`.
    let _query = TestConnection.from::<Post>().select(|(post,)| {
        lag(post.id.sum(), 1).over(|w| w.order_by(post.id.asc()))
    });
}
