use squealy_model::{
    ChangeRisk, ColumnModel, DatabaseModel, DatabasePlanStep, DdlExecutor, DiffPolicy,
    SchemaIntrospect, SchemaModel, SqlType, TableModel, TablePlanStep, apply_plan,
    classified_plan_steps, plan_from_database, plan_models, render_plan_sql,
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
                table: table("created"),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::SetTableComment {
                    before: None,
                    after: Some("new comment".to_owned()),
                },
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::AddColumn {
                    column: column("name", SqlType::Text),
                },
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
async fn apply_plan_renders_with_backend_and_executes_sql() {
    let desired = model_with_tables("public", vec![table("events")]);
    let actual = DatabaseModel { schemas: vec![] };
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan diff");
    let mut connection = TestConnection {
        model: actual,
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

#[derive(Debug)]
struct TestConnection {
    model: DatabaseModel,
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
