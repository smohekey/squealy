use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
struct LedgerRef(i32);

#[derive(Clone, Debug, PartialEq, Table)]
struct Entry<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    ledger_ref: C::Type<'scope, LedgerRef>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Plain<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    amount: C::Type<'scope, i32>,
}

fn main() {
    // `IN (subquery)` matches by value type, so a `LedgerRef` newtype operand may not be tested
    // against a subquery projecting a bare `i32` column — `OutputKind::Value` (`i32`) does not equal
    // the operand's value type (`LedgerRef`).
    let _query = TestConnection
        .from::<Entry>()
        .where_correlated(|(entry,), sub| {
            entry
                .ledger_ref
                .in_subquery(sub.from::<Plain>().select_subquery(|(plain,)| plain.amount))
        })
        .select(|(entry,)| entry.id);
}
