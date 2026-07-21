use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	name: C::Type<'scope, String>,
}

fn main() {
	let _query = TestConnection.to::<User>().all().update();
}
