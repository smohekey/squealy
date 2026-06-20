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
