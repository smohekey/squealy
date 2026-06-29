use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // The source's projected row type must match the target columns. Here the target column `name`
    // is `String` but the source projects `id` (`i32`), so the insert is rejected at compile time.
    let conn = TestConnection;
    let _query = conn.to::<User>().insert_select(
        |user| user.name,
        conn.from::<User>().select(|(user,)| user.id),
    );
}
