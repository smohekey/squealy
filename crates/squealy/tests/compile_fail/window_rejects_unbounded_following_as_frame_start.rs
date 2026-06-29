use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // SQL's `<frame start>` grammar forbids `UNBOUNDED FOLLOWING`, so `unbounded_following()` does not
    // implement `FrameStart`: passing it as the frame start is rejected at compile time.
    let _query = TestConnection.from::<Post>().select(|(post,)| {
        post.user_id
            .sum()
            .over(|w| w.order_by(post.id.asc()).rows(unbounded_following(), current_row()))
    });
}
