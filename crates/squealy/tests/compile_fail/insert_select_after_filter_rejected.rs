use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // `insert_select`'s rows come entirely from the source query, so it is only available on a fresh
    // builder. Applying a mutation filter first (`.all()`, like `.where_(..)`) would silently drop that
    // filter, so it is a compile error.
    let conn = TestConnection;
    let _query = conn.to::<User>().all().insert_select(
        |user| user.name,
        conn.from::<User>().select(|(user,)| user.name),
    );
}
