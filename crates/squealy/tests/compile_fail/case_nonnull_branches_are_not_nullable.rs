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
    // All branches are non-null (`i32` literals) and there is an `ELSE`, so the CASE result is the
    // non-null `i32` — `is_null` (only available on nullable expressions) must not type-check.
    let _query = TestConnection.from::<User>().where_(|user| {
        case()
            .when(user.id.greater_than(1), 1)
            .otherwise(0)
            .is_null()
    });
}
