use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // Named windows are gated to `SupportsNamedWindow` backends. The view-model backend
    // (`ModelConn` / `ModelBackend`) deliberately does not implement it, so declaring a named window
    // in a view body is a compile error — the view model does not yet carry window definitions.
    fn view_body(db: &'static ModelConn) {
        let _scope = db
            .from::<Post>()
            .window(|(post,)| Window::new().partition_by(post.user_id));
    }
    let _ = view_body;
}
