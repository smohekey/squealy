use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // A `WindowRef` handle is branded with a fresh, un-nameable lifetime unique to each `select_over`
    // call (the closure is `for<'brand>`-quantified). Stashing it in an outer variable to reuse in a
    // second query — which would render `OVER w0` with no matching `WINDOW w0 AS (…)` — is a compile
    // error: the branded lifetime cannot escape the closure.
    let mut leaked: Option<WindowRef<'static>> = None;
    let _first = TestConnection
        .from::<Post>()
        .window(|(post,)| Window::new().partition_by(post.user_id))
        .select_over(|(post,), w| {
            leaked = Some(w);
            post.user_id.sum().over_ref(w)
        });

    let _second = TestConnection
        .from::<Post>()
        .select(|(post,)| post.user_id.sum().over_ref(leaked.unwrap()));
}
