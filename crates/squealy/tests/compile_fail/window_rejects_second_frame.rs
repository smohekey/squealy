use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // A window frame is written last and only once: `.rows`/`.range` consume the frame slot (they
    // exist only on a frame-less window), so a second frame call is rejected at compile time.
    let _query = TestConnection.from::<Post>().select(|(post,)| {
        post.user_id.sum().over(|w| {
            w.order_by(post.id.asc())
                .rows(unbounded_preceding(), current_row())
                .range(preceding(1), following(1))
        })
    });
}
