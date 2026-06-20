//! End-to-end test of `#[derive(View)]`: the derive generates `SchemaView` metadata, the user writes
//! only the body, and the compile-time `Row` check ties the two together.

use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    active: C::Type<'scope, bool>,
}

// A view over the `users` table. `#[derive(View)]` generates the column/name/Row metadata from these
// fields; the body is written separately.
#[allow(dead_code)]
#[derive(View)]
#[schema(Public)]
struct ActiveUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
}

impl<'scope, C: ColumnMode> ViewDefinition for ActiveUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<User>()
            .where_(|user| user.active.equals(true))
            .project(|(user,)| (user.id, user.name))
    }
}

type ActiveUserMeta = ActiveUser<'static, ColumnExpr>;

#[test]
fn derive_generates_schema_view_metadata() {
    assert_eq!(<ActiveUserMeta as SchemaView>::view_name(), "active_users");
    assert_eq!(
        <ActiveUserMeta as SchemaView>::schema_name(),
        Some("public")
    );

    let columns = <ActiveUserMeta as SchemaView>::view_columns();
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[0].ty, SqlType::I32);
    assert!(!columns[0].nullable);
    assert_eq!(columns[1].name, "name");
    assert_eq!(columns[1].ty, SqlType::String);
}

#[test]
fn derived_view_lowers_its_body() {
    static CONN: ModelConn = ModelConn;
    let model = <ActiveUserMeta as ViewDefinition>::definition(&CONN).lower();

    assert_eq!(model.projection.len(), 2);
    let from = model.from.expect("FROM source");
    assert_eq!(from.name, "users");
    let filter = model.filter.expect("WHERE").0;
    assert!(filter.contains("TRUE"), "literal not inlined: {filter}");
}
