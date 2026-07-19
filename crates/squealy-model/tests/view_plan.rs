//! Incremental diff/plan coverage for views: adding, removing, and changing a view between the
//! desired and actual model must produce the right plan steps and risk classification.

use squealy::{ExprNode, ProjectionItem, SourceItem, SourceRef, ViewBody, ViewQueryModel};
use squealy_model::{
    ChangeRisk, DatabaseModel, DatabasePlanStep, DiffPolicy, SchemaModel, SqlType, TableModel,
    ViewColumnModel, ViewModel, classified_plan_steps, plan_models,
};

/// The mutable single-`SELECT` body of a view (these tests only build `SELECT` bodies).
fn select_mut(view: &mut ViewModel) -> &mut ViewQueryModel {
    match &mut view.query {
        ViewBody::Select(select) => select,
        ViewBody::Set { .. } | ViewBody::With { .. } => {
            panic!("expected a single-SELECT view body")
        }
    }
}

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
        query: ViewBody::Select(Box::new(ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: columns
                .iter()
                .map(|name| ProjectionItem {
                    output_name: (*name).to_owned(),
                    internal_alias: None,
                    expr: ExprNode::Column {
                        alias: "q0_0".to_owned(),
                        column: (*name).to_owned(),
                    },
                })
                .collect(),
            from: Some(SourceItem::Named(SourceRef {
                schema: Some("public".to_owned()),
                name: "users".to_owned(),
                alias: "q0_0".to_owned(),
            })),
            joins: Vec::new(),
            // The filter is a stand-in expression that differs between view variants so the diff sees a
            // body change.
            filter: Some(ExprNode::Literal(filter.to_owned())),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })),
    }
}

fn model(views: Vec<ViewModel>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: vec![users_table()],
            views,
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
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
    view.query = ViewBody::default();
    view
}

// An introspected view with a body that cannot be reconstructed still records its view-on-view
// dependencies (read from the catalogs), which drive drop ordering.
fn introspected_view_named(name: &str, columns: &[&str], depends_on: &[&str]) -> ViewModel {
    let mut view = introspected_view(columns);
    view.name = name.to_owned();
    select_mut(&mut view).dependencies = depends_on
        .iter()
        .map(|dependency| SourceRef {
            schema: Some("public".to_owned()),
            name: (*dependency).to_owned(),
            alias: (*dependency).to_owned(),
        })
        .collect();
    view
}

#[test]
fn dropping_a_view_for_a_column_change_drops_and_recreates_its_dependents() {
    // `parent` changes its column set (a REPLACE can't restructure columns, so it must DROP+recreate).
    // `child` selects from `parent` but keeps its own columns; it must still be dropped before `parent`
    // and recreated after, or `DROP parent` is rejected while `child` references it.
    let mut desired_parent = view("(q0_0.\"id\" > 0)", &["id", "name"]);
    desired_parent.name = "parent".to_owned();
    let mut desired_child = view("(q0_0.\"id\" > 0)", &["id"]);
    desired_child.name = "child".to_owned();
    // The desired child genuinely selects from parent, so the create phase orders parent before it.
    select_mut(&mut desired_child).from = Some(SourceItem::Named(SourceRef {
        schema: Some("public".to_owned()),
        name: "parent".to_owned(),
        alias: "q0_0".to_owned(),
    }));
    let desired = model(vec![desired_parent, desired_child]);

    let actual = model(vec![
        introspected_view_named("parent", &["id"], &[]),
        introspected_view_named("child", &["id"], &["parent"]),
    ]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");
    let dropped: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::DropView { view, .. } => Some(view.name.as_str()),
            _ => None,
        })
        .collect();
    let created: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::CreateView { view, .. } => Some(view.name.as_str()),
            _ => None,
        })
        .collect();

    let child_drop = dropped
        .iter()
        .position(|name| *name == "child")
        .expect("child dropped");
    let parent_drop = dropped
        .iter()
        .position(|name| *name == "parent")
        .expect("parent dropped");
    assert!(
        child_drop < parent_drop,
        "the dependent must be dropped before its dependency: {dropped:?}"
    );

    let parent_create = created
        .iter()
        .position(|name| *name == "parent")
        .expect("parent recreated");
    let child_create = created
        .iter()
        .position(|name| *name == "child")
        .expect("child recreated");
    assert!(
        parent_create < child_create,
        "the dependency must be recreated before the dependent: {created:?}"
    );
}

#[test]
fn cross_schema_dependent_is_not_promoted_by_a_same_named_view() {
    // `report` (in `public`) selects from `auth.users`, while an unrelated `public.users` is removed.
    // Keying dependencies by name alone would treat `report` as a dependent of `public.users` and drop
    // it; it must be left untouched (a same-shape introspected view, so just a CREATE OR REPLACE).
    let mut report = introspected_view(&["id"]);
    report.name = "report".to_owned();
    select_mut(&mut report).dependencies = vec![SourceRef {
        schema: Some("auth".to_owned()),
        name: "users".to_owned(),
        alias: "users".to_owned(),
    }];

    let mut desired_report = view("(q0_0.\"id\" > 0)", &["id"]);
    desired_report.name = "report".to_owned();
    let desired = model(vec![desired_report]);
    let actual = model(vec![introspected_view_named("users", &["id"], &[]), report]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");
    let dropped: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::DropView { view, .. } => Some(view.name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        dropped.contains(&"users"),
        "users should be dropped: {dropped:?}"
    );
    assert!(
        !dropped.contains(&"report"),
        "a cross-schema dependent must not be promoted: {dropped:?}"
    );
}

#[test]
fn cross_schema_dependent_is_dropped_and_recreated() {
    // `reporting.child` selects from `public.parent`; `public.parent` changes its column set, so it
    // must DROP+recreate. The cross-schema dependent must be dropped before it and recreated after —
    // which a per-schema diff would miss entirely.
    fn schema(name: &str, views: Vec<ViewModel>) -> SchemaModel {
        SchemaModel {
            name: Some(name.to_owned()),
            tables: Vec::new(),
            views,
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }
    }

    let mut desired_parent = view("(q0_0.\"id\" > 0)", &["id", "name"]);
    desired_parent.name = "parent".to_owned();
    let mut desired_child = view("(q0_0.\"id\" > 0)", &["id"]);
    desired_child.name = "child".to_owned();
    select_mut(&mut desired_child).from = Some(SourceItem::Named(SourceRef {
        schema: Some("public".to_owned()),
        name: "parent".to_owned(),
        alias: "q0_0".to_owned(),
    }));

    let mut actual_child = introspected_view_named("child", &["id"], &[]);
    select_mut(&mut actual_child).dependencies = vec![SourceRef {
        schema: Some("public".to_owned()),
        name: "parent".to_owned(),
        alias: "parent".to_owned(),
    }];

    let desired = DatabaseModel {
        schemas: vec![
            schema("public", vec![desired_parent]),
            schema("reporting", vec![desired_child]),
        ],
    };
    let actual = DatabaseModel {
        schemas: vec![
            schema(
                "public",
                vec![introspected_view_named("parent", &["id"], &[])],
            ),
            schema("reporting", vec![actual_child]),
        ],
    };

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");
    let dropped: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::DropView { view, .. } => Some(view.name.as_str()),
            _ => None,
        })
        .collect();
    let created: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::CreateView { view, .. } => Some(view.name.as_str()),
            _ => None,
        })
        .collect();

    let child_drop = dropped
        .iter()
        .position(|name| *name == "child")
        .expect("cross-schema dependent dropped");
    let parent_drop = dropped
        .iter()
        .position(|name| *name == "parent")
        .expect("parent dropped");
    assert!(
        child_drop < parent_drop,
        "cross-schema dependent must drop before its dependency: {dropped:?}"
    );
    let parent_create = created
        .iter()
        .position(|name| *name == "parent")
        .expect("parent recreated");
    let child_create = created
        .iter()
        .position(|name| *name == "child")
        .expect("cross-schema dependent recreated");
    assert!(
        parent_create < child_create,
        "dependency must be recreated before the cross-schema dependent: {created:?}"
    );
}

#[test]
fn introspected_interdependent_views_drop_dependents_first() {
    // Two live views where `child` selects from `parent`; both are removed (absent from desired). The
    // plan must DROP `child` before `parent`, or the database rejects dropping a view still in use.
    // Introspected views carry no body, so this ordering relies on their recorded dependencies.
    let parent = introspected_view_named("parent", &["id"], &[]);
    let child = introspected_view_named("child", &["id"], &["parent"]);

    // Declared parent-first, so a correct order must come from the dependency, not declaration order.
    let desired = model(vec![]);
    let actual = model(vec![parent, child]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");
    let dropped: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::DropView { view, .. } => Some(view.name.as_str()),
            _ => None,
        })
        .collect();
    let child_at = dropped
        .iter()
        .position(|name| *name == "child")
        .expect("child dropped");
    let parent_at = dropped
        .iter()
        .position(|name| *name == "parent")
        .expect("parent dropped");
    assert!(
        child_at < parent_at,
        "the dependent view must be dropped before the one it selects from: {dropped:?}"
    );
}

#[test]
fn introspected_view_with_matching_columns_is_replaced_without_drop() {
    // An introspected view has no comparable body, so the desired definition is re-applied as a safe
    // CREATE OR REPLACE every run (no drop, since the column set is unchanged) — this is how a
    // body-only change reaches a live database.
    let desired = model(vec![view("(q0_0.\"id\" > 0)", &["id"])]);
    let actual = model(vec![introspected_view(&["id"])]);

    // A lone CREATE OR REPLACE is non-destructive, so the default policy allows it.
    let plan = plan_models(&desired, &actual, DiffPolicy::default()).expect("plan");

    assert_eq!(plan.steps.len(), 1, "expected a single replace: {plan:?}");
    assert!(matches!(plan.steps[0], DatabasePlanStep::CreateView { .. }));
}

#[test]
fn introspected_view_nullability_difference_replaces_without_drop() {
    // Introspected view nullability is unreliable (PostgreSQL `attnotnull` is usually false for view
    // outputs), so a difference only in nullability must not drop+recreate — it is a same-shape view,
    // so it gets a safe CREATE OR REPLACE.
    let desired = model(vec![view("(q0_0.\"id\" > 0)", &["id"])]);
    let mut actual_view = introspected_view(&["id"]);
    actual_view.columns[0].nullable = !desired.schemas[0].views[0].columns[0].nullable;
    let actual = model(vec![actual_view]);

    let plan = plan_models(&desired, &actual, DiffPolicy::default()).expect("plan");

    assert_eq!(plan.steps.len(), 1, "expected a single replace: {plan:?}");
    assert!(matches!(plan.steps[0], DatabasePlanStep::CreateView { .. }));
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
    select_mut(&mut child).filter = Some(ExprNode::Exists {
        negated: false,
        subquery: Box::new(ViewQueryModel {
            from: Some(SourceItem::Named(SourceRef {
                schema: Some("public".to_owned()),
                name: "parent".to_owned(),
                alias: "q1_0".to_owned(),
            })),
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
