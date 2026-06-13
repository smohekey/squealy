use squealy_model::{
    AppliedRefactorError, ChangeRisk, ColumnModel, DatabaseModel, DatabasePlanStep, DdlExecutor,
    DiffPolicy, RefactorLog, RefactorOperation, RenameColumn, RenameTable, SchemaIntrospect,
    SchemaModel, SchemaRefactorStore, SqlType, TableModel, TablePlanStep, apply_plan,
    classified_plan_steps, pending_refactors, plan_from_database,
    plan_from_database_with_refactors, plan_models, plan_models_with_refactors, render_plan_sql,
    repair_refactor_metadata,
};
use squealy_postgresql::Postgres;

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
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterColumn {
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

    let sql = render_plan_sql(&plan, &Postgres).expect("render plan");

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

    apply_plan(&plan, &Postgres, &mut connection)
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
        &Postgres,
        &mut connection,
    )
    .await
    .expect("apply empty plan");

    assert!(connection.executed.is_empty());
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

fn model_with_tables(schema: &str, tables: Vec<TableModel>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some(schema.to_owned()),
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
