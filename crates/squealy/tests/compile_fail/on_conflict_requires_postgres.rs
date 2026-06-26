use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
}

fn main() {
    // `on_conflict` (upsert) is gated to backends that support `INSERT ... ON CONFLICT`
    // (`OnConflictQueryBuilder` — PostgreSQL). The in-memory test backend stands in for a MySQL-like
    // dialect and does not implement it, so the upsert does not type-check.
    let _ = TestConnection
        .to::<User>()
        .name("Ada")
        .on_conflict(|user| user.id)
        .do_nothing();
}
