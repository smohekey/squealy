use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    title: C::Type<'scope, String>,
}

fn main() {
    // `IN (subquery)` requires the subquery to project a single column of the operand's value type.
    // Here the operand is `i32` (`user.id`) but the subquery projects a `String` (`post.title`), so
    // `Subquery<Output = i32>` is not satisfied.
    let _query = TestConnection
        .from::<User>()
        .where_correlated(|(user,), sub| {
            user.id.in_subquery(
                sub.from::<Post>()
                    .select_subquery(|(post,)| post.title),
            )
        })
        .select(|(user,)| user.id);
}
