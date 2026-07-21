use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    let query = TestConnection
        .from::<User>()
        .where_(|user| user.name.equals(param::<UserName>()))
        .select(|(user,)| user.id);

    let _rows = query.fetch();
}
