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
    // Every COALESCE argument must share one value type. The first argument fixes it to `i32`, so a
    // `&str` fallback does not type-check.
    let _query = TestConnection
        .from::<User>()
        .select(|(user,)| coalesce(user.id).or_else("nope").end());
}
