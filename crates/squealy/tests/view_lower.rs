//! Validates that a typed query built against the canonical `ModelConn` lowers into the neutral
//! `ViewQueryModel` with literals inlined and structure captured.

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

#[test]
fn lowers_filtered_projection_with_inlined_literals() {
    let conn = ModelConn;
    let query = conn
        .from::<User>()
        .where_(|user| user.active.equals(true))
        .project(|(user,)| (user.id, user.name));

    let model = lower_view(&query);

    // Two projected columns. `output_name` is the builder's SELECT alias; a view's public column
    // names come from its declared `ViewColumnModel`, applied as an explicit column list in the DDL.
    assert_eq!(model.projection.len(), 2);
    assert!(model.projection[0].expr.0.contains("\"id\""));
    assert!(model.projection[1].expr.0.contains("\"name\""));

    let from = model.from.expect("a FROM source");
    assert_eq!(from.schema.as_deref(), Some("public"));
    assert_eq!(from.name, "users");

    // The `true` literal is inlined (no `$1`/`?` placeholder) and the filter references the alias.
    let filter = model.filter.expect("a WHERE filter").0;
    assert!(filter.contains("TRUE"), "literal not inlined: {filter}");
    assert!(
        filter.contains(&format!("{}.\"active\"", from.alias)),
        "filter does not reference the source alias: {filter}"
    );
    assert!(
        !filter.contains('$') && !filter.contains('?'),
        "view body must not contain bind placeholders: {filter}"
    );
}

#[test]
fn lowers_group_by_having_and_aggregate() {
    let conn = ModelConn;
    let query = conn
        .from::<User>()
        .group_by(|(user,)| user.name)
        .having(|(user,)| user.id.count().greater_than(1i64))
        .project(|(user,)| (user.name, user.id.count()));

    let model = lower_view(&query);

    assert_eq!(model.group_by.len(), 1);
    assert!(model.group_by[0].0.contains("\"name\""));
    let having = model.having.expect("a HAVING clause").0;
    assert!(having.contains("COUNT"), "aggregate not rendered: {having}");
    assert!(having.contains('1'), "literal not inlined: {having}");
}

#[test]
fn lowers_distinct_select_and_count_distinct() {
    let conn = ModelConn;

    // `SELECT DISTINCT` view body: the flag is captured on the model (it was previously dropped).
    let distinct = conn
        .from::<User>()
        .distinct()
        .project(|(user,)| (user.id, user.name));
    let distinct_model = lower_view(&distinct);
    assert!(
        distinct_model.distinct,
        "distinct flag not captured in the view model"
    );

    // Aggregate DISTINCT inside a (non-distinct) view body renders inside the call.
    let counted = conn
        .from::<User>()
        .project(|(user,)| user.id.count().distinct());
    let counted_model = lower_view(&counted);
    assert!(
        !counted_model.distinct,
        "select-level distinct must not be set for a plain count(distinct) view"
    );
    assert!(
        counted_model.projection[0]
            .expr
            .0
            .contains("COUNT(DISTINCT "),
        "count distinct not rendered in view body: {}",
        counted_model.projection[0].expr.0
    );
}

// A hand-written `ViewDefinition` (what `#[derive(View)]` will generate) walks through the object-safe
// `ViewDef` into a `ViewModel`, exercising the compile-time `Row` match between declared columns and
// the body's projection.
struct ActiveUsers;

impl SchemaView for ActiveUsers {
    type Row = (i32, String);

    fn schema_name() -> Option<&'static str> {
        Some("public")
    }

    fn view_name() -> &'static str {
        "active_users"
    }

    fn view_columns() -> Vec<ViewColumnModel> {
        vec![
            ViewColumnModel {
                name: "id".to_owned(),
                ty: SqlType::I32,
                nullable: false,
            },
            ViewColumnModel {
                name: "name".to_owned(),
                ty: SqlType::String,
                nullable: false,
            },
        ]
    }
}

impl ViewDefinition for ActiveUsers {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<User>()
            .where_(|user| user.active.equals(true))
            .project(|(user,)| (user.id, user.name))
    }
}

#[test]
fn view_definition_walks_into_a_view_model() {
    let view: &dyn ViewDef = &ActiveUsers;
    assert_eq!(view.name(), "active_users");
    assert_eq!(view.schema_name(), Some("public"));

    let columns = view.columns();
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[1].name, "name");

    let query = view.definition_model();
    assert_eq!(query.projection.len(), 2);
    let filter = query.filter.expect("WHERE").0;
    assert!(filter.contains("TRUE"), "literal not inlined: {filter}");
}
