use squealy::*;
use squealy_sqlite::SqliteConnection;

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
    // `intersect_all` / `except_all` are gated to `SupportsIntersectExceptAll` backends. SQLite only
    // allows `ALL` after `UNION`, so it deliberately does not implement the marker — calling
    // `intersect_all` is a compile error rather than emitting `INTERSECT ALL` that SQLite cannot
    // parse. (`union`/`intersect`/`except` are ungated and remain available.)
    fn rejected(conn: &SqliteConnection) {
        let _set = conn
            .from::<User>()
            .select(|(user,)| (user.id, user.name))
            .intersect_all(conn.from::<User>().select(|(user,)| (user.id, user.name)));
    }
    let _ = rejected;
}
