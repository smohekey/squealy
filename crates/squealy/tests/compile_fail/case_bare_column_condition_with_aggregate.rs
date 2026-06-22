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
    // A bare column in a WHEN condition combined with an aggregate THEN cannot be expressed in one
    // CASE (a column and an aggregate cannot mix in a single expression, like `COUNT(id) + id`).
    let _query = TestConnection
        .from::<User>()
        .select(|(user,)| case().when(user.id.greater_than(0), user.id.count()).otherwise(0i64));
}
