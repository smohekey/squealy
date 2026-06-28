use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // An untargeted `FOR UPDATE` cannot lock the nullable side of an outer join, so a row lock over a
    // LEFT/RIGHT/FULL join is rejected at compile time.
    let _query = TestConnection
        .from::<User>()
        .left_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .for_update()
        .select(|(user, _post)| user.id);
}
