use squealy::{ExprNode, SourceRef, ViewColumnModel, ViewModel, ViewQueryModel};
use squealy_model::{
    AppliedRefactorError, CastColumn, ChangeRisk, CheckModel, ColumnModel, DatabaseModel,
    DatabasePlanStep, DdlExecutor, DiffPolicy, IndexModel, PlanApplyOptions, RefactorLog,
    RefactorOperation, RenameColumn, RenameTable, SchemaIntrospect, SchemaModel,
    SchemaRefactorStore, SqlType, TableModel, TablePlanStep, apply_plan, apply_plan_with_options,
    classified_plan_steps, pending_refactors, plan_from_database,
    plan_from_database_with_refactors, plan_models, plan_models_with_refactors, render_plan_sql,
    render_plan_with_options, repair_refactor_metadata,
};
use squealy_postgresql::Postgres;

fn check_expr(sql: &str) -> squealy::ExprNode {
    squealy_parse::Reader::new(squealy_parse::SqlDialect::Generic)
        .read_check_expression(sql)
        .unwrap()
}

#[test]
fn plan_models_flattens_diff_changes_in_order() {
    let mut desired_events = table("events");
    desired_events.comment = Some("new comment".to_owned());
    desired_events.columns = vec![column("id", SqlType::I32), column("name", SqlType::Text)];

    let mut actual_events = table("events");
    actual_events.columns = vec![column("id", SqlType::I32)];

    let desired = model_with_tables("public", vec![table("created"), desired_events.clone()]);
    let actual = model_with_tables("public", vec![actual_events]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan diff");

    assert_eq!(
        plan.steps,
        vec![
            DatabasePlanStep::CreateTable {
                schema: Some("public".to_owned()),
                table: Box::new(table("created")),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::SetTableComment {
                    before: None,
                    after: Some("new comment".to_owned()),
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AddColumn {
                    column: column("name", SqlType::Text),
                }),
            },
        ]
    );
}

#[test]
fn plan_models_applies_policy_before_returning_steps() {
    let mut desired_events = table("events");
    desired_events.columns = vec![column("name", SqlType::Text)];
    let actual_events = table("events");

    let error = plan_models(
        &model_with_tables("public", vec![desired_events]),
        &model_with_tables("public", vec![actual_events]),
        DiffPolicy::default(),
    )
    .unwrap_err();

    assert_eq!(error.blocked.len(), 1);
    assert_eq!(error.blocked[0].risk, ChangeRisk::Ambiguous);
}

#[test]
fn plan_models_with_refactors_turns_table_drop_add_into_rename() {
    let desired = model_with_tables("public", vec![table("users")]);
    let actual = model_with_tables("public", vec![table("app_users")]);
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameTable(RenameTable {
            id: "rename-users".to_owned(),
            schema: Some("public".to_owned()),
            from: "app_users".to_owned(),
            to: "users".to_owned(),
        })],
    };

    let plan = plan_models_with_refactors(&desired, &actual, &refactors, DiffPolicy::default())
        .expect("renames are safe refactors");

    assert_eq!(
        plan.steps,
        vec![DatabasePlanStep::RenameTable {
            refactor_id: Some("rename-users".to_owned()),
            schema: Some("public".to_owned()),
            from: "app_users".to_owned(),
            to: "users".to_owned(),
        }]
    );
}

#[test]
fn plan_models_with_refactors_keeps_table_changes_after_rename() {
    let mut desired_users = table("users");
    desired_users.columns = vec![column("name", SqlType::Text)];
    let actual_users = table("app_users");
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameTable(RenameTable {
            id: "rename-users".to_owned(),
            schema: Some("public".to_owned()),
            from: "app_users".to_owned(),
            to: "users".to_owned(),
        })],
    };

    let plan = plan_models_with_refactors(
        &model_with_tables("public", vec![desired_users]),
        &model_with_tables("public", vec![actual_users]),
        &refactors,
        DiffPolicy::ALLOW_ALL,
    )
    .expect("plan renamed table with follow-up changes");

    assert_eq!(
        plan.steps,
        vec![
            DatabasePlanStep::RenameTable {
                refactor_id: Some("rename-users".to_owned()),
                schema: Some("public".to_owned()),
                from: "app_users".to_owned(),
                to: "users".to_owned(),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "users".to_owned(),
                change: Box::new(TablePlanStep::AddColumn {
                    column: column("name", SqlType::Text),
                }),
            },
        ]
    );
}

#[test]
fn plan_models_with_refactors_turns_column_drop_add_into_rename() {
    let mut desired_events = table("events");
    desired_events.columns = vec![column("name", SqlType::Text)];
    let mut actual_events = table("events");
    actual_events.columns = vec![column("display_name", SqlType::Text)];
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };

    let plan = plan_models_with_refactors(
        &model_with_tables("public", vec![desired_events]),
        &model_with_tables("public", vec![actual_events]),
        &refactors,
        DiffPolicy::default(),
    )
    .expect("renames are safe refactors");

    assert_eq!(
        plan.steps,
        vec![DatabasePlanStep::AlterTable {
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            change: Box::new(TablePlanStep::RenameColumn {
                refactor_id: Some("rename-user-name".to_owned()),
                from: "display_name".to_owned(),
                to: "name".to_owned(),
                column_type: SqlType::Text,
            }),
        }]
    );
}

#[test]
fn plan_models_with_refactors_keeps_column_changes_after_rename() {
    let mut desired_events = table("events");
    let mut desired_name = column("name", SqlType::Text);
    desired_name.nullable = true;
    desired_events.columns = vec![desired_name.clone()];
    let mut actual_events = table("events");
    actual_events.columns = vec![column("display_name", SqlType::Text)];
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };

    let plan = plan_models_with_refactors(
        &model_with_tables("public", vec![desired_events]),
        &model_with_tables("public", vec![actual_events]),
        &refactors,
        DiffPolicy::ALLOW_ALL,
    )
    .expect("plan renamed column with follow-up changes");

    let mut renamed_before = column("name", SqlType::Text);
    renamed_before.nullable = false;

    assert_eq!(
        plan.steps,
        vec![
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::RenameColumn {
                    refactor_id: Some("rename-user-name".to_owned()),
                    from: "display_name".to_owned(),
                    to: "name".to_owned(),
                    column_type: SqlType::Text,
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterColumn {
                    type_cast: None,
                    before: renamed_before,
                    after: desired_name,
                }),
            },
        ]
    );
}

#[test]
fn plan_models_with_refactors_leaves_unmatched_refactors_blocked_by_policy() {
    let mut desired_events = table("events");
    desired_events.columns = vec![column("name", SqlType::Text)];
    let actual_events = table("events");
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };

    let error = plan_models_with_refactors(
        &model_with_tables("public", vec![desired_events]),
        &model_with_tables("public", vec![actual_events]),
        &refactors,
        DiffPolicy::default(),
    )
    .unwrap_err();

    assert_eq!(error.blocked.len(), 1);
    assert_eq!(error.blocked[0].risk, ChangeRisk::Ambiguous);
}

#[test]
fn pending_refactors_filters_valid_recorded_renames() {
    let mut events = table("events");
    events.columns = vec![column("name", SqlType::Text)];
    let actual = model_with_tables("public", vec![events]);
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };

    let pending = pending_refactors(&refactors, &["rename-user-name".to_owned()], &actual)
        .expect("recorded refactor should validate");

    assert!(pending.is_empty());
}

#[test]
fn pending_refactors_rejects_recorded_rename_when_old_column_still_exists() {
    let mut events = table("events");
    events.columns = vec![
        column("display_name", SqlType::Text),
        column("name", SqlType::Text),
    ];
    let actual = model_with_tables("public", vec![events]);
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };

    let error = pending_refactors(&refactors, &["rename-user-name".to_owned()], &actual)
        .expect_err("stale applied-refactor metadata should fail");

    assert_eq!(
        error,
        AppliedRefactorError::RenameColumnSourceStillExists {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            column: "display_name".to_owned(),
        }
    );
}

#[test]
fn plan_steps_are_classified_individually() {
    let desired = DatabaseModel { schemas: vec![] };
    let actual = model_with_tables("public", vec![table("events")]);

    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan diff");

    assert_eq!(
        classified_plan_steps(&plan)
            .into_iter()
            .map(|step| step.risk)
            .collect::<Vec<_>>(),
        vec![ChangeRisk::Destructive, ChangeRisk::Destructive]
    );
}

#[test]
fn render_plan_sql_delegates_to_backend_plan_renderer() {
    let desired = model_with_tables("public", vec![table("events")]);
    let actual = DatabaseModel { schemas: vec![] };
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan diff");

    let sql = render_plan_sql(&plan, &desired, &Postgres).expect("render plan");

    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS \"public\";\n\
CREATE TABLE \"public\".\"events\" (\n\n);"
    );
}

#[tokio::test]
async fn plan_from_database_introspects_actual_model_before_planning() {
    let desired = model_with_tables("public", vec![table("events")]);
    let mut connection = TestConnection {
        model: DatabaseModel { schemas: vec![] },
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };

    let plan = plan_from_database(&desired, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan from database");

    assert_eq!(plan.steps.len(), 2);
    assert!(matches!(
        plan.steps[0],
        DatabasePlanStep::CreateSchema {
            schema: Some(ref schema),
        } if schema == "public"
    ));
}

#[tokio::test]
async fn plan_from_database_with_refactors_rejects_stale_recorded_refactors() {
    let mut desired_events = table("events");
    desired_events.columns = vec![column("name", SqlType::Text)];
    let desired = model_with_tables("public", vec![desired_events]);

    let mut actual_events = table("events");
    actual_events.columns = vec![column("display_name", SqlType::Text)];
    let mut connection = TestConnection {
        model: model_with_tables("public", vec![actual_events]),
        applied_refactor_ids: vec!["rename-user-name".to_owned()],
        executed: Vec::new(),
    };
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };

    let error = plan_from_database_with_refactors(
        &desired,
        &refactors,
        &mut connection,
        DiffPolicy::default(),
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        squealy_model::PlanFromDatabaseError::AppliedRefactor(
            AppliedRefactorError::RenameColumnTargetMissing { .. }
        )
    ));
}

#[tokio::test]
async fn repair_refactor_metadata_records_valid_missing_refactors() {
    let mut events = table("events");
    events.columns = vec![column("name", SqlType::Text)];
    let mut connection = TestConnection {
        model: model_with_tables("public", vec![events]),
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };

    let report = repair_refactor_metadata(&refactors, &mut connection)
        .await
        .expect("repair refactor metadata");

    assert_eq!(report.recorded, vec!["rename-user-name".to_owned()]);
    assert!(report.already_recorded.is_empty());
    assert_eq!(
        connection.applied_refactor_ids,
        vec!["rename-user-name".to_owned()]
    );
}

#[tokio::test]
async fn repair_refactor_metadata_rejects_unapplied_refactors() {
    let mut events = table("events");
    events.columns = vec![column("display_name", SqlType::Text)];
    let mut connection = TestConnection {
        model: model_with_tables("public", vec![events]),
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-user-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };

    let error = repair_refactor_metadata(&refactors, &mut connection)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        squealy_model::RepairRefactorMetadataError::AppliedRefactor(
            AppliedRefactorError::RenameColumnTargetMissing { .. }
        )
    ));
    assert!(connection.applied_refactor_ids.is_empty());
}

#[tokio::test]
async fn apply_plan_renders_with_backend_and_executes_sql() {
    let desired = model_with_tables("public", vec![table("events")]);
    let actual = DatabaseModel { schemas: vec![] };
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan diff");
    let mut connection = TestConnection {
        model: actual,
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };

    apply_plan(&plan, &desired, &Postgres, &mut connection)
        .await
        .expect("apply plan");

    assert_eq!(connection.executed.len(), 1);
    assert!(
        connection.executed[0].contains("CREATE SCHEMA IF NOT EXISTS \"public\""),
        "{}",
        connection.executed[0]
    );
    assert!(
        connection.executed[0].contains("CREATE TABLE \"public\".\"events\""),
        "{}",
        connection.executed[0]
    );
}

#[tokio::test]
async fn apply_plan_does_not_execute_empty_plans() {
    let mut connection = TestConnection {
        model: DatabaseModel { schemas: vec![] },
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };

    apply_plan(
        &squealy_model::DatabasePlan::default(),
        &DatabaseModel::default(),
        &Postgres,
        &mut connection,
    )
    .await
    .expect("apply empty plan");

    assert!(connection.executed.is_empty());
}

#[tokio::test]
async fn apply_plan_with_concurrent_indexes_splits_index_creation() {
    let plan = squealy_model::DatabasePlan {
        steps: vec![
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AddColumn {
                    column: column("slug", SqlType::Text),
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AddIndex {
                    index: IndexModel {
                        name: "idx_events_slug".to_owned(),
                        columns: vec!["slug".to_owned()],
                        expressions: Vec::new(),
                        include_columns: Vec::new(),
                        unique: false,
                        method: None,
                        directions: Vec::new(),
                        nulls: Vec::new(),
                        collations: Vec::new(),
                        operator_classes: Vec::new(),
                        predicate: None,
                    },
                }),
            },
        ],
    };
    let mut connection = TestConnection {
        model: DatabaseModel {
            schemas: Vec::new(),
        },
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };

    apply_plan_with_options(
        &plan,
        &DatabaseModel::default(),
        &Postgres,
        &mut connection,
        PlanApplyOptions {
            concurrent_indexes: true,
        },
    )
    .await
    .expect("apply plan");

    // Transactional steps run first, the concurrent index second (on its own execution).
    assert_eq!(connection.executed.len(), 2);
    assert!(
        connection.executed[0].contains("ADD COLUMN \"slug\"")
            && !connection.executed[0].contains("CONCURRENTLY"),
        "transactional batch: {}",
        connection.executed[0]
    );
    assert!(
        connection.executed[1].contains("CREATE INDEX CONCURRENTLY \"idx_events_slug\""),
        "concurrent batch: {}",
        connection.executed[1]
    );
}

#[test]
fn render_plan_with_options_reports_the_concurrent_form_it_will_apply() {
    let plan = squealy_model::DatabasePlan {
        steps: vec![
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AddColumn {
                    column: column("slug", SqlType::Text),
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AddIndex {
                    index: IndexModel {
                        name: "idx_events_slug".to_owned(),
                        columns: vec!["slug".to_owned()],
                        expressions: Vec::new(),
                        include_columns: Vec::new(),
                        unique: false,
                        method: None,
                        directions: Vec::new(),
                        nulls: Vec::new(),
                        collations: Vec::new(),
                        operator_classes: Vec::new(),
                        predicate: None,
                    },
                }),
            },
        ],
    };

    // The dry-run report must reflect what `apply_plan_with_options` actually applies: the index is
    // built `CONCURRENTLY` outside the transaction, after the transactional `ADD COLUMN`.
    let concurrent = render_plan_with_options(
        &plan,
        &DatabaseModel::default(),
        &Postgres,
        PlanApplyOptions {
            concurrent_indexes: true,
        },
    )
    .expect("render concurrent report");
    let add_column = concurrent.find("ADD COLUMN \"slug\"").expect("add column");
    let concurrent_index = concurrent
        .find("CREATE INDEX CONCURRENTLY \"idx_events_slug\"")
        .expect("concurrent index");
    assert!(
        add_column < concurrent_index,
        "transactional step should render before the concurrent index: {concurrent}"
    );

    // Without the option it stays byte-identical to the plain renderer (plain, transactional index).
    let plain = render_plan_with_options(
        &plan,
        &DatabaseModel::default(),
        &Postgres,
        PlanApplyOptions::default(),
    )
    .expect("render plain report");
    assert_eq!(
        plain,
        render_plan_sql(&plan, &DatabaseModel::default(), &Postgres).unwrap()
    );
    assert!(!plain.contains("CONCURRENTLY"), "plain report: {plain}");
}

#[derive(Debug)]
struct TestConnection {
    model: DatabaseModel,
    applied_refactor_ids: Vec<String>,
    executed: Vec<String>,
}

#[derive(Debug)]
struct TestError;

impl std::fmt::Display for TestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("test error")
    }
}

impl std::error::Error for TestError {}

impl SchemaIntrospect for TestConnection {
    type Error = TestError;

    async fn introspect_database(&mut self) -> Result<DatabaseModel, TestError> {
        Ok(self.model.clone())
    }

    // Mimics PostgreSQL, where `String` and `Text` both render to `text` and introspect as `String`.
    fn canonical_sql_type(&self, ty: &SqlType) -> SqlType {
        match ty {
            SqlType::Text => SqlType::String,
            other => other.clone(),
        }
    }

    // Stand-in for the backend's real expression normalizer: strips whitespace and parentheses and
    // lowercases, enough to map an "authored" and a "deparsed" form of the same expression together.
    fn canonical_index_predicate(&self, predicate: &str) -> String {
        predicate
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '(' && *c != ')')
            .collect::<String>()
            .to_ascii_lowercase()
    }

    // Stand-in for a backend that re-parses a legacy-package `Raw` index expression into the structural
    // form live introspection produces, so the two converge (PostgreSQL does this in its own dialect).
    fn canonical_index_expression(&self, expression: squealy::ExprNode) -> squealy::ExprNode {
        match expression {
            squealy::ExprNode::Raw(sql) => {
                squealy_parse::Reader::new(squealy_parse::SqlDialect::Generic)
                    .read_index_expression_or_raw(&sql)
            }
            other => other,
        }
    }
}

#[tokio::test]
async fn plan_from_database_canonicalizes_backend_equivalent_types() {
    // The live schema introspects the column as `String`; the desired model authored it as `Text`.
    // On a backend where they are the same physical type, this must not produce a type-change.
    let live = model_with_tables(
        "public",
        vec![{
            let mut events = table("events");
            events.columns = vec![column("note", SqlType::String)];
            events
        }],
    );
    let desired = model_with_tables(
        "public",
        vec![{
            let mut events = table("events");
            events.columns = vec![column("note", SqlType::Text)];
            events
        }],
    );
    let mut connection = TestConnection {
        model: live,
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };

    let plan = plan_from_database(&desired, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan");

    assert!(
        plan.is_empty(),
        "String/Text are equivalent on this backend; expected no changes, got {:?}",
        plan.steps
    );
}

#[tokio::test]
async fn plan_from_database_canonicalizes_view_column_types() {
    // The live schema introspects a view column as `String` (with an empty body); the desired view
    // authored it as `Text`. On a backend where they are the same physical type, comparing the view
    // by columns must canonicalize both sides, so an unchanged view does not churn a drop+recreate.
    let view = |ty: SqlType, introspected: bool| ViewModel {
        name: "active".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "note".to_owned(),
            ty,
            nullable: false,
        }],
        query: if introspected {
            ViewQueryModel::default()
        } else {
            ViewQueryModel {
                from: Some(SourceRef {
                    schema: Some("public".to_owned()),
                    name: "events".to_owned(),
                    alias: "q0_0".to_owned(),
                }),
                ..ViewQueryModel::default()
            }
        },
    };
    let schema = |view: ViewModel| DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: Vec::new(),
            views: vec![view],
        }],
    };
    let mut connection = TestConnection {
        model: schema(view(SqlType::String, true)),
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };

    let plan = plan_from_database(
        &schema(view(SqlType::Text, false)),
        &mut connection,
        DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("plan");

    // An introspected view is always re-applied as a safe CREATE OR REPLACE; the point here is that
    // String/Text being equivalent on this backend keeps it a same-shape replace, not a destructive
    // drop+recreate from a spurious column-type change.
    assert!(
        !plan
            .steps
            .iter()
            .any(|step| matches!(step, DatabasePlanStep::DropView { .. })),
        "String/Text are equivalent on this backend; the view must not drop+recreate, got {:?}",
        plan.steps
    );
    assert!(
        plan.steps
            .iter()
            .any(|step| matches!(step, DatabasePlanStep::CreateView { .. })),
        "expected a CREATE OR REPLACE for the introspected view, got {:?}",
        plan.steps
    );
}

#[tokio::test]
async fn plan_from_database_canonicalizes_predicates_and_checks_on_both_sides() {
    // The live schema deparses an index predicate and a CHECK expression one way; the desired model
    // authored them another. `canonicalize_model` must run on BOTH sides for them to converge — if
    // only the desired side were canonicalized, the introspected form would still differ and churn.
    let model = |predicate: &str, check: &str| {
        model_with_tables(
            "public",
            vec![{
                let mut t = table("t");
                t.columns = vec![
                    column("status", SqlType::I32),
                    column("score", SqlType::I32),
                ];
                t.indexes = vec![IndexModel {
                    name: "uq_t_status".to_owned(),
                    columns: vec!["status".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: true,
                    method: None,
                    directions: Vec::new(),
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: Some(predicate.to_owned()),
                }];
                t.checks = vec![CheckModel {
                    name: "ck_t_score".to_owned(),
                    expression: check_expr(check),
                    validation: None,
                    enforcement: None,
                }];
                t
            }],
        )
    };
    let live = model("(status = 1)", "(score > 0)");
    let desired = model("STATUS = 1", "score>0");
    let mut connection = TestConnection {
        model: live,
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };

    let plan = plan_from_database(&desired, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan");

    assert!(
        plan.is_empty(),
        "predicate/CHECK differ only in surface form; expected no changes, got {:?}",
        plan.steps
    );
}

#[tokio::test]
async fn plan_from_database_canonicalizes_legacy_index_expression() {
    // A legacy package carries an index-key expression verbatim as `Raw` (the old string form), while
    // live introspection now lowers the same expression to a structural node. `canonical_index_expression`
    // must run on both sides so the two forms converge instead of churning an `AlterIndex` every publish.
    let index = |expression: ExprNode| {
        model_with_tables(
            "public",
            vec![{
                let mut t = table("t");
                t.columns = vec![column("status", SqlType::Text)];
                t.indexes = vec![IndexModel {
                    name: "idx_t_lower_status".to_owned(),
                    columns: Vec::new(),
                    expressions: vec![expression],
                    include_columns: Vec::new(),
                    unique: false,
                    method: None,
                    directions: Vec::new(),
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: None,
                }];
                t
            }],
        )
    };
    let live = index(ExprNode::ScalarFn {
        func: squealy::ScalarFunc::Lower,
        args: vec![ExprNode::BareColumn {
            column: "status".to_owned(),
        }],
    });
    let desired = index(ExprNode::Raw("lower(status)".to_owned()));
    let mut connection = TestConnection {
        model: live,
        applied_refactor_ids: Vec::new(),
        executed: Vec::new(),
    };

    let plan = plan_from_database(&desired, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan");

    assert!(
        plan.is_empty(),
        "legacy Raw index expression matches the structural introspected one; expected no changes, got {:?}",
        plan.steps
    );
}

impl SchemaRefactorStore for TestConnection {
    type Error = TestError;

    async fn applied_refactor_ids(&mut self) -> Result<Vec<String>, TestError> {
        Ok(self.applied_refactor_ids.clone())
    }

    async fn record_applied_refactor_ids(&mut self, ids: &[String]) -> Result<(), TestError> {
        for id in ids {
            if !self.applied_refactor_ids.contains(id) {
                self.applied_refactor_ids.push(id.clone());
            }
        }
        self.applied_refactor_ids.sort();
        Ok(())
    }
}

impl DdlExecutor for TestConnection {
    type Error = TestError;

    async fn execute_ddl(&mut self, sql: &str) -> Result<(), TestError> {
        self.executed.push(sql.to_owned());
        Ok(())
    }
}

#[test]
fn cast_column_refactor_renders_using_clause_on_a_type_change() {
    let mut desired = table("events");
    desired.columns = vec![column(
        "total",
        SqlType::Decimal {
            precision: 12,
            scale: 2,
        },
    )];
    let mut actual = table("events");
    actual.columns = vec![column("total", SqlType::Text)];

    let refactors = RefactorLog {
        operations: vec![RefactorOperation::CastColumn(CastColumn {
            id: "cast-total".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            column: "total".to_owned(),
            using: "total::numeric".to_owned(),
        })],
    };

    let desired_model = model_with_tables("public", vec![desired]);
    let plan = plan_models_with_refactors(
        &desired_model,
        &model_with_tables("public", vec![actual]),
        &refactors,
        DiffPolicy::ALLOW_ALL,
    )
    .expect("type change is allowed by ALLOW_ALL");

    let sql = render_plan_sql(&plan, &desired_model, &Postgres).expect("render");
    assert!(
        sql.contains("ALTER COLUMN \"total\" TYPE numeric(12,2) USING total::numeric"),
        "{sql}"
    );
}

#[test]
fn cast_column_applies_after_rename_regardless_of_log_order() {
    let mut desired = table("events");
    desired.columns = vec![column(
        "total",
        SqlType::Decimal {
            precision: 12,
            scale: 2,
        },
    )];
    let mut actual = table("events");
    actual.columns = vec![column("old_total", SqlType::Text)];

    // The cast is listed BEFORE the rename that creates the column it targets; the cast must still
    // attach to the rename's type-change step.
    let refactors = RefactorLog {
        operations: vec![
            RefactorOperation::CastColumn(CastColumn {
                id: "cast-total".to_owned(),
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                column: "total".to_owned(),
                using: "old_total::numeric".to_owned(),
            }),
            RefactorOperation::RenameColumn(RenameColumn {
                id: "rename-total".to_owned(),
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                from: "old_total".to_owned(),
                to: "total".to_owned(),
            }),
        ],
    };

    let desired_model = model_with_tables("public", vec![desired]);
    let plan = plan_models_with_refactors(
        &desired_model,
        &model_with_tables("public", vec![actual]),
        &refactors,
        DiffPolicy::ALLOW_ALL,
    )
    .expect("rename + cast allowed by ALLOW_ALL");

    let sql = render_plan_sql(&plan, &desired_model, &Postgres).expect("render");
    assert!(sql.contains("RENAME COLUMN"), "expected a rename: {sql}");
    assert!(
        sql.contains("USING old_total::numeric"),
        "cast must survive being listed before the rename: {sql}"
    );
}

fn model_with_tables(schema: &str, tables: Vec<TableModel>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some(schema.to_owned()),
            views: Vec::new(),
            tables,
        }],
    }
}

fn table(name: &str) -> TableModel {
    TableModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![],
        primary_key: None,
        foreign_keys: vec![],
        uniques: vec![],
        checks: vec![],
        indexes: vec![],
    }
}

fn column(name: &str, ty: SqlType) -> ColumnModel {
    ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty,
        collation: None,
        nullable: false,
        default: None,
        identity: None,
        generated: None,
    }
}
