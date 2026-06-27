use squealy::*;
use squealy_test::TestConnection;

// Set operations require both arms to produce the same row type. A `(i32, String)` left arm and a
// `(i32, i32)` right arm are column-incompatible and must be rejected.

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
    let _ = TestConnection
        .from::<User>()
        .select(|(u,)| (u.id, u.name))
        .union(TestConnection.from::<User>().select(|(u,)| (u.id, u.id)));
}
