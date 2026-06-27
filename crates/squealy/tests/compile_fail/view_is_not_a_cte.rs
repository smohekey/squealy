use squealy::*;

// CTEs are one-directional the other way too: a `#[derive(View)]` type is persisted as `CREATE VIEW`
// and is never inlined, so it is not a `CteDefinition`. Lowering it as a CTE body
// (`cte_definition_model`) must fail.

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
struct ActiveUserView<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for ActiveUserView<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<User>()
            .where_(|user| user.active.equals(true))
            .project(|(user,)| (user.id, user.name))
    }
}

fn main() {
    // A view is not a CTE definition, so it cannot be lowered as a CTE body.
    let _ = cte_definition_model::<ActiveUserView>();
}
