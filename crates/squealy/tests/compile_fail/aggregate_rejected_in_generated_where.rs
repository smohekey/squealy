use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    total: C::Type<'scope, i64>,
}

fn main() {
    // The derived write-builder's `where_` must reject aggregate predicates too.
    let _query = TestConnection
        .to::<Counter>()
        .total(0i64)
        .where_(|counter| counter.id.count().greater_than(0i64));
}
