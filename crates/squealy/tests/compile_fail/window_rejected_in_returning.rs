use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // Window functions are not valid in a RETURNING clause; RETURNING requires `ProjectionColumns`,
    // which a window AST does not implement.
    let _query = TestConnection
        .to::<Post>()
        .user_id(1)
        .insert_returning(|post| row_number().over(|w| w.order_by(post.id.asc())));
}
