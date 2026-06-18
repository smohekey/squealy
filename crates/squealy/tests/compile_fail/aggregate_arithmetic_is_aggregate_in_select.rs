use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // `COUNT` is non-null `i64`, so `count() + 1` is an aggregate *expression*; mixing it with a
    // bare column is still invalid without GROUP BY (the binary is classified aggregate).
    let _query = TestConnection
        .from::<User>()
        .select(|(user,)| (user.name, user.id.count() + 1i64));
}
