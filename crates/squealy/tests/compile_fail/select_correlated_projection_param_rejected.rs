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
    // A projected scalar subquery renders before the outer FROM; a runtime `param` inside it can't
    // be bound by the top-level query, so `select_correlated` requires the projection to be free of
    // runtime params (`ProjectionParams<Params = HNil>`). This projects a scalar subquery whose
    // WHERE contains a `param`, so it is rejected rather than silently emitting an unbound
    // placeholder.
    let _query = TestConnection.from::<Account>().select_correlated(|(_account,), sub| {
        scalar_subquery(
            sub.from::<Ledger>()
                .where_(|ledger| ledger.amount.equals(param::<LedgerAmount>()))
                .select_subquery(|(ledger,)| ledger.amount),
        )
    });
}
