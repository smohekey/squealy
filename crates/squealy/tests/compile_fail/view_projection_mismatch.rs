// A view's `definition` body must project to the same row type as the declared columns. Here the
// view declares `(id: i32, name: String)` but the body projects `(id, active)` = `(i32, bool)`, so the
// `impl ViewSelect<Row = <Self as SchemaView>::Row>` bound fails to hold.

use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    active: C::Type<'scope, bool>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(View)]
#[schema(Public)]
struct ActiveUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for ActiveUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        // Wrong: projects `active` (bool) where the declared column is `name` (String).
        db.from::<User>().project(|(user,)| (user.id, user.active))
    }
}

fn main() {}
