use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Ledger<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    amount: C::Type<'scope, i32>,
}

fn main() {
    // The projection-param guard also covers the sourceless `QueryBuilder::select`: projecting a
    // scalar subquery whose WHERE contains a runtime `param` is rejected, rather than rendering a
    // placeholder the top-level query can't bind.
    let sq = TestConnection
        .from::<Ledger>()
        .where_(|ledger| ledger.amount.equals(param::<LedgerAmount>()))
        .select_subquery(|(ledger,)| ledger.amount);

    let _query = TestConnection.select(scalar_subquery(sq));
}
