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
    // Every CASE branch (THEN/ELSE) must share one value type. The first `when` fixes it to `i32`, so
    // an `&str` ELSE does not type-check.
    let _query = TestConnection
        .from::<User>()
        .select(|(user,)| case().when(user.id.greater_than(1), 1).otherwise("nope"));
}
