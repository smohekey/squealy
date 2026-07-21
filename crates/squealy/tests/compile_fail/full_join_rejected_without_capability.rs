use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
}

fn main() {
    // `full_join` is gated to `SupportsFullJoin` backends. `TestBackend` (like MySQL) does not
    // implement it, so `FULL JOIN` — which the backend cannot render — is rejected at compile time.
    let _query = TestConnection
        .from::<User>()
        .full_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| (user.id, post.id));
}
