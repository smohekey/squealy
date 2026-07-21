use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // Window functions are evaluated after grouping, so a window may not appear in a HAVING
    // predicate; a window AST has no `ExprColumns` classification, so HAVING rejects it.
    let _query = TestConnection
        .from::<Post>()
        .group_by(|(post,)| post.user_id)
        .having(|(post,)| {
            row_number()
                .over(|w| w.order_by(post.id.asc()))
                .greater_than(1_i64)
        })
        .select(|(post,)| post.user_id);
}
