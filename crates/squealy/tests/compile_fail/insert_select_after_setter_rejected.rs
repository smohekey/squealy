use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // `insert_select` takes its values from the source query, so mixing it with a column setter would
    // silently drop the setter's value. It is therefore only available on a fresh builder (no setters).
    let conn = TestConnection;
    let _query = conn.to::<User>().name("Ada").insert_select(
        |user| user.name,
        conn.from::<User>().select(|(user,)| user.name),
    );
}
