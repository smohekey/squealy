//! Foundation test of `#[derive(CTE)]`: the derive generates `SchemaCte` metadata + a queryable
//! projection, the user writes only the body, and the compile-time `Row` check ties the two together.
//! (Rendering the `WITH` clause is a later increment.)

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

// A CTE over the `users` table. `#[derive(CTE)]` generates the column/name/Row metadata + the
// queryable projection; the body is written separately (parameter-free, like a view body).
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

type ActiveUserMeta = ActiveUser<'static, ColumnExpr>;

#[test]
fn derive_generates_schema_cte_metadata() {
    assert_eq!(<ActiveUserMeta as SchemaCte>::cte_name(), "active_users");

    let columns = <ActiveUserMeta as SchemaCte>::cte_columns();
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[0].ty, SqlType::I32);
    assert!(!columns[0].nullable);
    assert_eq!(columns[1].name, "name");
    assert_eq!(columns[1].ty, SqlType::String);
}

#[test]
fn cte_body_lowers_to_a_model() {
    let model = cte_definition_model::<ActiveUserMeta>();
    assert_eq!(model.projection.len(), 2);
    let Some(SourceItem::Named(from)) = model.from.as_ref() else {
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

// `CteDef` is object-safe (so the WITH collector can hold `&dyn CteDef`).
#[test]
fn cte_def_is_object_safe() {
    fn _assert(_: &dyn CteDef) {}
}
