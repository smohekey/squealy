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
    #[view]
    active_users: ActiveUser<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct AppDatabase {
    public: Public,
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
    let SourceItem::Named(from) = model.from.expect("FROM source") else {
        panic!("expected a named FROM source");
    };
    assert_eq!(from.name, "users");
    assert!(matches!(
        model.filter.expect("WHERE"),
        ExprNode::Compare {
            op: CompareOp::Equals,
            ..
        }
    ));
}

#[test]
fn database_walk_includes_views() {
    let model = DatabaseModel::from_database::<AppDatabase>();
    let schema = &model.schemas[0];
    assert_eq!(schema.name.as_deref(), Some("public"));
    assert_eq!(schema.tables.len(), 1);
    assert_eq!(schema.tables[0].name, "users");

    assert_eq!(schema.views.len(), 1);
    let view = &schema.views[0];
    assert_eq!(view.name, "active_users");
    assert_eq!(view.columns.len(), 2);
    assert_eq!(view.columns[0].name, "id");
    assert_eq!(view.columns[1].name, "name");
    assert_eq!(view.query.projection.len(), 2);
    let Some(SourceItem::Named(from)) = view.query.from.as_ref() else {
        panic!("expected a named FROM source");
    };
    assert_eq!(from.name, "users");
    assert!(matches!(
        view.query.filter.as_ref().unwrap(),
        ExprNode::Compare {
            op: CompareOp::Equals,
            ..
        }
    ));
}

// The view is queryable as a FROM source, including from another view's body (view-on-view).
#[test]
fn view_is_queryable_as_a_from_source() {
    static CONN: ModelConn = ModelConn;
    let query = CONN
        .from::<ActiveUser>()
        .where_(|active| active.id.greater_than(0))
        .project(|(active,)| (active.id, active.name));

    let model = lower_view(&query);
    let Some(SourceItem::Named(from)) = model.from.as_ref() else {
        panic!("expected a named FROM source");
    };
    assert_eq!(from.name, "active_users");
    assert_eq!(model.projection.len(), 2);
}
