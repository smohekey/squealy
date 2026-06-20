use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

fn main() {
    // A window definition's ORDER BY term may not itself be a window function (nested windows are
    // rejected by the backends); `order_by` requires a `NonAggregateAst` term.
    let _query = TestConnection.from::<Post>().select(|(post,)| {
        row_number().over(|w| {
            w.order_by(
                row_number()
                    .over(|inner| inner.order_by(post.id.asc()))
                    .asc(),
            )
        })
    });
}
