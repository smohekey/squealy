//! Incremental diff/plan coverage for views: adding, removing, and changing a view between the
//! desired and actual model must produce the right plan steps and risk classification.

use squealy::{ExprNode, ProjectionItem, SourceRef, ViewQueryModel};
use squealy_model::{
    ChangeRisk, DatabaseModel, DatabasePlanStep, DiffPolicy, SchemaModel, SqlType, TableModel,
    ViewColumnModel, ViewModel, classified_plan_steps, plan_models,
};

fn users_table() -> TableModel {
    TableModel {
        name: "users".to_owned(),
        comment: None,
        columns: Vec::new(),
        primary_key: None,
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
    }
}

fn view(filter: &str, columns: &[&str]) -> ViewModel {
    ViewModel {
        name: "active_users".to_owned(),
        comment: None,
        columns: columns
            .iter()
            .map(|name| ViewColumnModel {
                name: (*name).to_owned(),
                ty: SqlType::I32,
                nullable: false,
            })
            .collect(),
        query: ViewQueryModel {
            projection: columns
                .iter()
                .map(|name| ProjectionItem {
                    output_name: (*name).to_owned(),
                    expr: ExprNode::Column {
                        alias: "q0_0".to_owned(),
                        column: (*name).to_owned(),
                    },
                })
                .collect(),
            from: Some(SourceRef {
                schema: Some("public".to_owned()),
                name: "users".to_owned(),
                alias: "q0_0".to_owned(),
            }),
            joins: Vec::new(),
            // The filter is a stand-in expression that differs between view variants so the diff sees a
            // body change.
            filter: Some(ExprNode::Literal(filter.to_owned())),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        },
    }
}

fn model(views: Vec<ViewModel>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: vec![users_table()],
            views,
        }],
    }
}

#[test]
fn adding_a_view_plans_a_safe_create() {
    let desired = model(vec![view("(q0_0.\"id\" > 0)", &["id"])]);
    let actual = model(vec![]);

    let plan = plan_models(&desired, &actual, DiffPolicy::default()).expect("plan");
    let classified = classified_plan_steps(&plan);

    assert_eq!(classified.len(), 1);
    assert_eq!(classified[0].risk, ChangeRisk::Safe);
    assert!(matches!(
        classified[0].step,
        DatabasePlanStep::CreateView { ref view, .. } if view.name == "active_users"
    ));
}

#[test]
fn removing_a_view_plans_a_destructive_drop() {
    let desired = model(vec![]);
    let actual = model(vec![view("(q0_0.\"id\" > 0)", &["id"])]);

    // A drop is destructive, so the default policy blocks it.
    plan_models(&desired, &actual, DiffPolicy::default()).unwrap_err();

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");
    let classified = classified_plan_steps(&plan);
    assert_eq!(classified.len(), 1);
    assert_eq!(classified[0].risk, ChangeRisk::Destructive);
    assert!(matches!(
        classified[0].step,
        DatabasePlanStep::DropView { .. }
    ));
}

#[test]
fn changing_only_the_body_plans_a_single_safe_replace() {
    let desired = model(vec![view("(q0_0.\"id\" > 10)", &["id"])]);
    let actual = model(vec![view("(q0_0.\"id\" > 0)", &["id"])]);

    let plan = plan_models(&desired, &actual, DiffPolicy::default()).expect("plan");
    // Same columns, different body -> one CREATE OR REPLACE, no drop.
    assert_eq!(plan.steps.len(), 1);
    assert!(matches!(plan.steps[0], DatabasePlanStep::CreateView { .. }));
}

#[test]
fn changing_the_column_set_plans_drop_then_create() {
    let desired = model(vec![view("(q0_0.\"id\" > 0)", &["id", "name"])]);
    let actual = model(vec![view("(q0_0.\"id\" > 0)", &["id"])]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");
    assert_eq!(plan.steps.len(), 2);
    assert!(matches!(plan.steps[0], DatabasePlanStep::DropView { .. }));
    assert!(matches!(plan.steps[1], DatabasePlanStep::CreateView { .. }));
}

// A live-introspected view has no structural body (it can't be rebuilt from stored SQL). The diff
// must compare those by columns only, so re-introspecting an unchanged view plans nothing instead of
// a spurious CreateView every run.
fn introspected_view(columns: &[&str]) -> ViewModel {
    let mut view = view("ignored-body", columns);
    view.query = ViewQueryModel::default();
    view
}

#[test]
fn introspected_view_with_matching_columns_plans_nothing() {
    let desired = model(vec![view("(q0_0.\"id\" > 0)", &["id"])]);
    let actual = model(vec![introspected_view(&["id"])]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    assert!(
        plan.is_empty(),
        "an unchanged introspected view must not be recreated: {plan:?}"
    );
}

#[test]
fn introspected_view_nullability_difference_plans_nothing() {
    // Introspected view nullability is unreliable (PostgreSQL `attnotnull` is usually false for view
    // outputs), so a difference only in nullability must not churn a drop+recreate.
    let desired = model(vec![view("(q0_0.\"id\" > 0)", &["id"])]);
    let mut actual_view = introspected_view(&["id"]);
    actual_view.columns[0].nullable = !desired.schemas[0].views[0].columns[0].nullable;
    let actual = model(vec![actual_view]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    assert!(
        plan.is_empty(),
        "an introspected view that differs only in nullability must not be recreated: {plan:?}"
    );
}

#[test]
fn introspected_view_with_changed_columns_is_recreated() {
    let desired = model(vec![view("(q0_0.\"id\" > 0)", &["id", "name"])]);
    let actual = model(vec![introspected_view(&["id"])]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    // A column-set change can't use CREATE OR REPLACE, so it drops then recreates.
    assert_eq!(plan.steps.len(), 2);
    assert!(matches!(plan.steps[0], DatabasePlanStep::DropView { .. }));
    assert!(matches!(plan.steps[1], DatabasePlanStep::CreateView { .. }));
}

#[test]
fn subquery_only_view_dependency_orders_create_after_it() {
    // `child` references `parent` ONLY inside an EXISTS subquery in its filter (not in FROM/JOIN), so
    // the dependency is invisible unless the source walker recurses into subqueries.
    let mut parent = view("(q0_0.\"id\" > 0)", &["id"]);
    parent.name = "parent".to_owned();

    let mut child = view("(q0_0.\"id\" > 0)", &["id"]);
    child.name = "child".to_owned();
    child.query.filter = Some(ExprNode::Exists {
        negated: false,
        subquery: Box::new(ViewQueryModel {
            from: Some(SourceRef {
                schema: Some("public".to_owned()),
                name: "parent".to_owned(),
                alias: "q1_0".to_owned(),
            }),
            ..ViewQueryModel::default()
        }),
    });

    // Declared child-first, so a correct order must come from the dependency, not declaration order.
    let desired = model(vec![child, parent]);
    let actual = model(vec![]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");
    let created: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::CreateView { view, .. } => Some(view.name.as_str()),
            _ => None,
        })
        .collect();
    let parent_at = created
        .iter()
        .position(|name| *name == "parent")
        .expect("parent created");
    let child_at = created
        .iter()
        .position(|name| *name == "child")
        .expect("child created");
    assert!(
        parent_at < child_at,
        "parent must be created before the child whose subquery selects from it: {created:?}"
    );
}
