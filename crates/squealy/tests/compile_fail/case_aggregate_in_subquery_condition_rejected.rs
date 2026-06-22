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
#[schema(Public)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
    posts: Post<'static, ColumnName>,
}

fn main() {
    // An `in_subquery` condition whose operand is an aggregate (`COUNT`) cannot mix with a bare
    // column: the subquery condition is row-dependent, so combining it with the aggregate operand has
    // no `CombineTerm` (like `COUNT(id) + id`). Rejecting it prevents an ungrouped `COUNT(…)`.
    let _query = TestConnection.from::<User>().select_correlated(|(user,), sub| {
        case()
            .when(
                user.id.count().in_subquery(
                    sub.from::<Post>().select_subquery(|(post,)| post.id.count()),
                ),
                1,
            )
            .otherwise(0)
    });
}
