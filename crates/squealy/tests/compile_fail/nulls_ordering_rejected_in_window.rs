use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    score: C::Type<'scope, i32>,
}

fn main() {
    // `NULLS FIRST/LAST` is not offered inside a window `ORDER BY` (its MySQL emulation would have to
    // rewrite the `OVER (…)` ordering). `nulls_last()` yields an `OrderNullsTerm`, which a window's
    // `order_by` (which takes an `Order`) does not accept.
    let _query = TestConnection
        .from::<Post>()
        .select(|(post,)| post.id.sum().over(|window| window.order_by(post.score.asc().nulls_last())));
}
