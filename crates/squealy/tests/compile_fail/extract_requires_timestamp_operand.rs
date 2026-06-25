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
    // `extract` requires a timestamp/date operand (a `TimestampKind` value). An `i32` column is not a
    // timestamp, so it does not type-check.
    let _query = TestConnection
        .from::<User>()
        .select(|(user,)| extract(DateField::Year, user.id));
}
