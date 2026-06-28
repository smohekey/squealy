use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

fn main() {
    // A locking clause is invalid in a UNION/INTERSECT/EXCEPT input, so a row-locked select cannot be a
    // set operand (in either position).
    let locked = TestConnection
        .from::<User>()
        .for_update()
        .select(|(user,)| user.id);
    let other = TestConnection.from::<User>().select(|(user,)| user.id);
    let _set = locked.union(other);
}
