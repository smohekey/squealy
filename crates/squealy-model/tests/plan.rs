use squealy_model::{
    ChangeRisk, ColumnModel, DatabaseModel, DatabasePlanStep, DiffPolicy, SchemaModel, SqlType,
    TableModel, TablePlanStep, classified_plan_steps, plan_models,
};

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
