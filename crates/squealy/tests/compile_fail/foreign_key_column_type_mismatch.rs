use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i64>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Member<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    // `account_id` is `i32` but `Account::id` is `i64`: the foreign-key column types must match.
    #[column(references(Account::id))]
    account_id: C::Type<'scope, i32>,
}

fn main() {}
