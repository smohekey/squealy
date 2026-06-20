// A view body has no bind parameters — every value is inlined. A definition that uses a runtime
// `param()` must fail the `NoRuntimeParams` bound on `ViewSelect`, rather than silently dropping the
// placeholder and emitting invalid DDL.

use squealy::*;

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

#[allow(dead_code)]
#[derive(View)]
#[schema(Public)]
struct NamedUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for NamedUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        // Wrong: a view body cannot carry a runtime parameter.
        db.from::<User>()
            .where_(|user| user.name.equals(param::<UserName>()))
            .project(|(user,)| (user.id, user.name))
    }
}

fn main() {}
