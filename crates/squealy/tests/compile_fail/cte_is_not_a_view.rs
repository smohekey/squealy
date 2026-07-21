use squealy::*;

// CTEs are one-directional: a `#[derive(CTE)]` type is inlined as a `WITH` clause and is never
// persisted, so it has no view body. Trying to lower it as a view definition (the model walker's
// `view_definition_model`) must fail — a CTE does not implement `ViewDefinition`.

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
#[derive(CTE)]
struct ActiveUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> CteDefinition for ActiveUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaCte>::Row> {
        db.from::<User>()
            .where_(|user| user.active.equals(true))
            .project(|(user,)| (user.id, user.name))
    }
}

fn main() {
	// A CTE has no persistent view body, so it cannot be used as a view definition.
    let _ = view_definition_model::<ActiveUser>();
}
