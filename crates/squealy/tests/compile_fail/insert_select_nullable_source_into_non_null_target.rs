use squealy::*;
use squealy_test::TestConnection;

type MaybeLabel = Option<String>;

#[derive(Clone, Debug, PartialEq, Table)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    label: C::Type<'scope, MaybeLabel>,
}

fn main() {
    // Widening is one-way: a nullable source column (`label: Option<String>`) cannot fill a non-null
    // target column (`name: String`) — that would insert `NULL` into a `NOT NULL` column. (Coverage is
    // satisfied: `name` is the target, and the nullable `label` is omittable.)
    let conn = TestConnection;
    let _query = conn.to::<Account>().insert_select(
        |account| account.name,
        conn.from::<Account>().select(|(account,)| account.label),
    );
}
