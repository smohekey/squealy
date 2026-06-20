use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Ledger<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    amount: C::Type<'scope, i32>,
}

fn main() {
    // The projection-param guard also covers plain `select`: building the scalar subquery first and
    // projecting it through ordinary `select` is rejected too, so a runtime `param` in a projected
    // scalar subquery can never reach the SELECT list as an unbindable placeholder.
    let sq = TestConnection
        .from::<Ledger>()
        .where_(|ledger| ledger.amount.equals(param::<LedgerAmount>()))
        .select_subquery(|(ledger,)| ledger.amount);

    let _query = TestConnection
        .from::<Account>()
        .select(move |(_account,)| scalar_subquery(sq));
}
