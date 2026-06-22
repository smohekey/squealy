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
    // The CASE's WHEN condition contains an aggregate (`COUNT`), so the CASE is an aggregate
    // projection and cannot be mixed with a bare column in an ungrouped select.
    let _query = TestConnection.from::<User>().select(|(user,)| {
        (
            user.name,
            case()
                .when(user.id.count().greater_than(1i64), 1i64)
                .otherwise(0i64),
        )
    });
}
