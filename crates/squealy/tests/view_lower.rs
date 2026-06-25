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

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
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
    assert!(matches!(&model.projection[0].expr, ExprNode::Column { column, .. } if column == "id"));
    assert!(
        matches!(&model.projection[1].expr, ExprNode::Column { column, .. } if column == "name")
    );

    let from = model.from.expect("a FROM source");
    assert_eq!(from.schema.as_deref(), Some("public"));
    assert_eq!(from.name, "users");

    // The filter is `active = TRUE` structurally: the column references the source alias and the `true`
    // literal is inlined as `TRUE` (no bind placeholder, just a `Literal` node).
    match model.filter.expect("a WHERE filter") {
        ExprNode::Compare {
            op: CompareOp::Equals,
            left,
            right,
        } => {
            assert!(
                matches!(*left, ExprNode::Column { ref alias, ref column } if *alias == from.alias && column == "active")
            );
            assert_eq!(*right, ExprNode::Literal("TRUE".to_owned()));
        }
        other => panic!("unexpected filter: {other:?}"),
    }
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
    assert!(matches!(&model.group_by[0], ExprNode::Column { column, .. } if column == "name"));

    // HAVING is `COUNT(id) > 1`: a comparison of an aggregate against an inlined `1` literal.
    match model.having.expect("a HAVING clause") {
        ExprNode::Compare {
            op: CompareOp::GreaterThan,
            left,
            right,
        } => {
            assert!(matches!(
                *left,
                ExprNode::Aggregate {
                    func: AggregateFunc::Count,
                    ..
                }
            ));
            assert_eq!(*right, ExprNode::Literal("1".to_owned()));
        }
        other => panic!("unexpected having: {other:?}"),
    }
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
        matches!(
            &counted_model.projection[0].expr,
            ExprNode::Aggregate {
                func: AggregateFunc::Count,
                distinct: true,
                ..
            }
        ),
        "count(distinct) not captured in view body: {:?}",
        counted_model.projection[0].expr
    );
}

#[test]
fn lowers_right_and_full_joins() {
    use squealy::JoinKind;
    let conn = ModelConn;

    let right = conn
        .from::<User>()
        .right_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .project(|(user, post)| (user.id, post.id));
    let right_model = lower_view(&right);
    assert_eq!(right_model.joins.len(), 1);
    assert_eq!(right_model.joins[0].kind, JoinKind::Right);

    // `full_join` compiles against the model backend (`ModelBackend: SupportsFullJoin`).
    let full = conn
        .from::<User>()
        .full_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .project(|(user, post)| (user.id, post.id));
    let full_model = lower_view(&full);
    assert_eq!(full_model.joins[0].kind, JoinKind::Full);

    // `cross_join` lowers to a `Cross` join with no `ON` condition.
    let cross = conn
        .from::<User>()
        .cross_join::<Post>()
        .project(|(user, post)| (user.id, post.id));
    let cross_model = lower_view(&cross);
    assert_eq!(cross_model.joins.len(), 1);
    assert_eq!(cross_model.joins[0].kind, JoinKind::Cross);
    assert_eq!(cross_model.joins[0].on, None);
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
    assert!(matches!(
        query.filter.expect("WHERE"),
        ExprNode::Compare {
            op: CompareOp::Equals,
            ..
        }
    ));
}

// A window function lowers into a structural `ExprNode::Window`, with its partition/order lists and
// direction captured (so it can be rendered per-dialect).
#[test]
fn lowers_window_function() {
    let conn = ModelConn;
    let query = conn.from::<User>().project(|(user,)| {
        row_number().over(|w| w.partition_by(user.name).order_by(user.id.asc()))
    });

    let model = lower_view(&query);

    assert_eq!(model.projection.len(), 1);
    match &model.projection[0].expr {
        ExprNode::Window {
            func,
            partition_by,
            order_by,
            ..
        } => {
            assert!(matches!(func, WindowFunc::RowNumber));
            assert_eq!(partition_by.len(), 1);
            assert!(
                matches!(&partition_by[0], ExprNode::Column { column, .. } if column == "name")
            );
            assert_eq!(order_by.len(), 1);
            assert!(matches!(&order_by[0].expr, ExprNode::Column { column, .. } if column == "id"));
            assert!(matches!(order_by[0].direction, OrderDirection::Asc));
        }
        other => panic!("expected a window node, got {other:?}"),
    }
}

#[test]
fn lowers_case_expression_to_ir() {
    let conn = ModelConn;
    let query = conn
        .from::<User>()
        .project(|(user,)| case().when(user.active.equals(true), 1).otherwise(0));
    let model = lower_view(&query);
    match &model.projection[0].expr {
        ExprNode::Case { arms, else_, .. } => {
            assert_eq!(arms.len(), 1);
            assert!(matches!(&*arms[0].when, ExprNode::Compare { .. }));
            assert!(matches!(&*arms[0].then, ExprNode::Literal(_)));
            assert!(matches!(else_.as_deref(), Some(ExprNode::Literal(_))));
        }
        other => panic!("expected CASE node, got {other:?}"),
    }
}

#[test]
fn lowers_coalesce_and_nullif_to_ir() {
    let conn = ModelConn;
    let coalesce_q = conn
        .from::<User>()
        .project(|(user,)| coalesce(user.id).or_else(0).end());
    match &lower_view(&coalesce_q).projection[0].expr {
        ExprNode::Coalesce { args, .. } => assert_eq!(args.len(), 2),
        other => panic!("expected COALESCE node, got {other:?}"),
    }

    let nullif_q = conn.from::<User>().project(|(user,)| nullif(user.id, 0));
    assert!(matches!(
        &lower_view(&nullif_q).projection[0].expr,
        ExprNode::Nullif { .. }
    ));
}

#[test]
fn lowers_simple_case_to_ir() {
    let conn = ModelConn;
    let query = conn
        .from::<User>()
        .project(|(user,)| case_of(user.id).when(1, 10).otherwise(0));
    match &lower_view(&query).projection[0].expr {
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            ..
        } => {
            assert!(matches!(&**operand, ExprNode::Column { .. }));
            assert_eq!(arms.len(), 1);
            assert!(matches!(&*arms[0].when, ExprNode::Literal(_)));
            assert!(else_.is_some());
        }
        other => panic!("expected simple CASE node, got {other:?}"),
    }
}

#[test]
fn lowers_string_functions_to_ir() {
    let conn = ModelConn;
    let lower_q = conn.from::<User>().project(|(user,)| lower(user.name));
    match &lower_view(&lower_q).projection[0].expr {
        ExprNode::ScalarFn { func, args } => {
            assert_eq!(*func, ScalarFunc::Lower);
            assert_eq!(args.len(), 1);
            assert!(matches!(&args[0], ExprNode::Column { .. }));
        }
        other => panic!("expected ScalarFn node, got {other:?}"),
    }

    let sub_q = conn
        .from::<User>()
        .project(|(user,)| substring(user.name, 1, 3));
    match &lower_view(&sub_q).projection[0].expr {
        ExprNode::ScalarFn { func, args } => {
            assert_eq!(*func, ScalarFunc::Substring);
            assert_eq!(args.len(), 3);
        }
        other => panic!("expected ScalarFn node, got {other:?}"),
    }
}

#[cfg(feature = "systemtime")]
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct TimedEvent<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    created: C::Type<'scope, std::time::SystemTime>,
}

#[cfg(feature = "systemtime")]
#[test]
fn lowers_datetime_functions_to_ir() {
    use std::time::SystemTime;
    let conn = ModelConn;

    let now_q = conn
        .from::<TimedEvent>()
        .project(|(_e,)| now::<SystemTime>());
    assert!(matches!(
        &lower_view(&now_q).projection[0].expr,
        ExprNode::Now
    ));

    let extract_q = conn
        .from::<TimedEvent>()
        .project(|(e,)| extract(DateField::Year, e.created));
    match &lower_view(&extract_q).projection[0].expr {
        ExprNode::Extract {
            field,
            operand,
            result,
            timezone,
        } => {
            assert_eq!(*field, DateField::Year);
            assert_eq!(*result, Some(SqlType::I64));
            assert_eq!(*timezone, None);
            assert!(matches!(**operand, ExprNode::Column { .. }));
        }
        other => panic!("expected Extract node, got {other:?}"),
    }

    let trunc_q = conn
        .from::<TimedEvent>()
        .project(|(e,)| date_trunc(DateField::Day, e.created));
    match &lower_view(&trunc_q).projection[0].expr {
        ExprNode::DateTrunc {
            unit,
            operand,
            timezone,
        } => {
            assert_eq!(*unit, DateField::Day);
            assert_eq!(*timezone, None);
            assert!(matches!(**operand, ExprNode::Column { .. }));
        }
        other => panic!("expected DateTrunc node, got {other:?}"),
    }

    // The timezone-explicit variants carry the zone into the IR.
    let extract_at_q = conn
        .from::<TimedEvent>()
        .project(|(e,)| extract_at(DateField::Hour, e.created, "UTC"));
    match &lower_view(&extract_at_q).projection[0].expr {
        ExprNode::Extract {
            field, timezone, ..
        } => {
            assert_eq!(*field, DateField::Hour);
            assert_eq!(timezone.as_deref(), Some("UTC"));
        }
        other => panic!("expected Extract node, got {other:?}"),
    }

    let trunc_at_q = conn
        .from::<TimedEvent>()
        .project(|(e,)| date_trunc_at(DateField::Day, e.created, "UTC"));
    match &lower_view(&trunc_at_q).projection[0].expr {
        ExprNode::DateTrunc { unit, timezone, .. } => {
            assert_eq!(*unit, DateField::Day);
            assert_eq!(timezone.as_deref(), Some("UTC"));
        }
        other => panic!("expected DateTrunc node, got {other:?}"),
    }

    // `extract(Second)` flows through the existing Extract node (field = Second).
    let second_q = conn
        .from::<TimedEvent>()
        .project(|(e,)| extract(DateField::Second, e.created));
    match &lower_view(&second_q).projection[0].expr {
        ExprNode::Extract { field, .. } => assert_eq!(*field, DateField::Second),
        other => panic!("expected Extract node, got {other:?}"),
    }

    // `extract_second` lowers to the dedicated ExtractSecond node (f64 result).
    let frac_q = conn
        .from::<TimedEvent>()
        .project(|(e,)| extract_second(e.created));
    match &lower_view(&frac_q).projection[0].expr {
        ExprNode::ExtractSecond { operand, result } => {
            assert_eq!(*result, Some(SqlType::F64));
            assert!(matches!(**operand, ExprNode::Column { .. }));
        }
        other => panic!("expected ExtractSecond node, got {other:?}"),
    }
}
