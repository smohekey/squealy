use squealy::*;
use squealy_test::TestConnection;

// A boolean-backed `ColumnType` newtype must be excluded from `MIN`/`MAX` just like a primitive
// `bool` column: the derived `AggregateScalar` impl is gated on the wrapped type, and `bool` is not
// an aggregate scalar (PostgreSQL has no `min`/`max(boolean)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
struct Flag(bool);

#[derive(Clone, Debug, PartialEq, Table)]
struct Toggle<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    flag: C::Type<'scope, Flag>,
}

fn main() {
    let _query = TestConnection
        .from::<Toggle>()
        .select(|(toggle,)| toggle.flag.max());
}
