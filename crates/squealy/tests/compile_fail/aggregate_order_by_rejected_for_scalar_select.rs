use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // Ordering a scalar projection by an aggregate (`SELECT id ... ORDER BY COUNT(id)`) is invalid
    // without GROUP BY: the order class is `OrderAggregate`, incompatible with a scalar projection.
    let _query = TestConnection
        .from::<User>()
        .order_by(|(user,)| user.id.count().asc())
        .select(|(user,)| user.id);
}
