use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    let _insert = TestConnection
        .to::<User>()
        .id(1)
        .name(param::<UserName>())
        .insert();
}
