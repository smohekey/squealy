use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // One-shot `insert()` collects only static binds, so an INSERT…SELECT whose source carries a
    // runtime `param` cannot be executed this way (it would leave a placeholder with no value). The
    // render (`to_sql`) is fine; only `.insert()` is rejected.
    let conn = TestConnection;
    let _ = conn
        .to::<User>()
        .insert_select(
            |user| user.name,
            conn.from::<User>()
                .where_(|user| user.id.equals(param::<UserId>()))
                .select(|(user,)| user.name),
        )
        .insert();
}
