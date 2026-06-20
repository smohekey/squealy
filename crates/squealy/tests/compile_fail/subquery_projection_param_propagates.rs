use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    balance: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Ledger<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    amount: C::Type<'scope, i32>,
}

fn main() {
    // A runtime `param` in the subquery's SELECT list is a real runtime parameter of the outer
    // query, so the outer query is not directly executable — it must be prepared and bound. Calling
    // `fetch()` (which requires `NoRuntimeParams`) is therefore rejected, proving the projection
    // param is threaded into the outer query's `Params` rather than silently dropped.
    let query = TestConnection
        .from::<Account>()
        .where_correlated(|(account,), sub| {
            account.balance.in_subquery(
                sub.from::<Ledger>()
                    .select_subquery(|(ledger,)| ledger.amount + param::<LedgerAmount>()),
            )
        })
        .select(|(account,)| account.id);

    let _rows = query.fetch();
}
