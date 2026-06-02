use squealy_model::{
    CheckModel, ColumnModel, Constraint, DatabaseDiffChange, DatabaseModel, ForeignKeyAction,
    ForeignKeyModel, IndexModel, SchemaModel, SqlType, TableDiffChange, TableModel, diff_models,
};

#[test]
fn identical_models_have_empty_diff() {
    let model = model_with_tables("public", vec![table("events")]);

    let diff = diff_models(&model, &model);

    assert!(diff.is_empty());
    assert!(diff.changes.is_empty());
}

#[test]
fn diff_reports_schema_and_table_creation() {
    let desired = model_with_tables("public", vec![table("events")]);
    let actual = DatabaseModel { schemas: vec![] };

    let diff = diff_models(&desired, &actual);

    assert_eq!(
        diff.changes,
        vec![
            DatabaseDiffChange::CreateSchema {
                schema: Some("public".to_owned()),
            },
            DatabaseDiffChange::CreateTable {
                schema: Some("public".to_owned()),
                table: table("events"),
            },
        ]
    );
}

#[test]
fn diff_reports_table_drop_before_schema_drop() {
    let desired = DatabaseModel { schemas: vec![] };
    let actual = model_with_tables("public", vec![table("events")]);

    let diff = diff_models(&desired, &actual);

    assert_eq!(
        diff.changes,
        vec![
            DatabaseDiffChange::DropTable {
                schema: Some("public".to_owned()),
                table: table("events"),
            },
            DatabaseDiffChange::DropSchema {
                schema: Some("public".to_owned()),
            },
        ]
    );
}

#[test]
fn diff_reports_table_add_drop_and_alter() {
    let mut desired_events = table("events");
    desired_events.comment = Some("desired comment".to_owned());
    desired_events.columns = vec![column("id", SqlType::I32), column("name", SqlType::Text)];

    let mut actual_events = table("events");
    actual_events.comment = Some("actual comment".to_owned());
    actual_events.columns = vec![
        ColumnModel {
            ty: SqlType::I64,
            ..column("id", SqlType::I32)
        },
        column("obsolete", SqlType::Text),
    ];

    let desired = model_with_tables("public", vec![desired_events.clone(), table("created")]);
    let actual = model_with_tables("public", vec![actual_events.clone(), table("dropped")]);

    let diff = diff_models(&desired, &actual);

    assert_eq!(
        diff.changes,
        vec![
            DatabaseDiffChange::CreateTable {
                schema: Some("public".to_owned()),
                table: table("created"),
            },
            DatabaseDiffChange::DropTable {
                schema: Some("public".to_owned()),
                table: table("dropped"),
            },
            DatabaseDiffChange::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                changes: vec![
                    TableDiffChange::SetTableComment {
                        before: Some("actual comment".to_owned()),
                        after: Some("desired comment".to_owned()),
                    },
                    TableDiffChange::AlterColumn {
                        before: ColumnModel {
                            ty: SqlType::I64,
                            ..column("id", SqlType::I32)
                        },
                        after: column("id", SqlType::I32),
                    },
                    TableDiffChange::AddColumn {
                        column: column("name", SqlType::Text),
                    },
                    TableDiffChange::DropColumn {
                        column: column("obsolete", SqlType::Text),
                    },
                ],
            },
        ]
    );
}

#[test]
fn diff_reports_named_constraint_and_index_changes() {
    let mut desired = table("events");
    desired.primary_key = Some(constraint("pk_events", &["id"]));
    desired.uniques = vec![constraint("uq_events_name", &["name"])];
    desired.foreign_keys = vec![foreign_key("fk_events_user_id", "users")];
    desired.checks = vec![check("ck_events_id", "id > 0")];
    desired.indexes = vec![index("idx_events_name", &["name"])];

    let mut actual = table("events");
    actual.primary_key = Some(constraint("pk_events", &["event_id"]));
    actual.uniques = vec![constraint("uq_events_slug", &["slug"])];
    actual.foreign_keys = vec![ForeignKeyModel {
        on_delete: Some(ForeignKeyAction::Cascade),
        ..foreign_key("fk_events_user_id", "users")
    }];
    actual.checks = vec![check("ck_events_id", "event_id > 0")];
    actual.indexes = vec![
        index("idx_events_name", &["name", "id"]),
        index("idx_events_obsolete", &["obsolete"]),
    ];

    let desired_model = model_with_tables("public", vec![desired]);
    let actual_model = model_with_tables("public", vec![actual]);

    let diff = diff_models(&desired_model, &actual_model);

    assert_eq!(
        diff.changes,
        vec![DatabaseDiffChange::AlterTable {
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            changes: vec![
                TableDiffChange::AlterPrimaryKey {
                    before: constraint("pk_events", &["event_id"]),
                    after: constraint("pk_events", &["id"]),
                },
                TableDiffChange::AddUnique {
                    constraint: constraint("uq_events_name", &["name"]),
                },
                TableDiffChange::DropUnique {
                    constraint: constraint("uq_events_slug", &["slug"]),
                },
                TableDiffChange::AlterForeignKey {
                    before: ForeignKeyModel {
                        on_delete: Some(ForeignKeyAction::Cascade),
                        ..foreign_key("fk_events_user_id", "users")
                    },
                    after: foreign_key("fk_events_user_id", "users"),
                },
                TableDiffChange::AlterCheck {
                    before: check("ck_events_id", "event_id > 0"),
                    after: check("ck_events_id", "id > 0"),
                },
                TableDiffChange::AlterIndex {
                    before: index("idx_events_name", &["name", "id"]),
                    after: index("idx_events_name", &["name"]),
                },
                TableDiffChange::DropIndex {
                    index: index("idx_events_obsolete", &["obsolete"]),
                },
            ],
        }]
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

fn constraint(name: &str, columns: &[&str]) -> Constraint {
    Constraint {
        name: name.to_owned(),
        columns: columns.iter().map(|column| (*column).to_owned()).collect(),
    }
}

fn foreign_key(name: &str, references_table: &str) -> ForeignKeyModel {
    ForeignKeyModel {
        name: name.to_owned(),
        columns: vec!["user_id".to_owned()],
        references_schema: Some("public".to_owned()),
        references_table: references_table.to_owned(),
        references_columns: vec!["id".to_owned()],
        match_type: None,
        deferrability: None,
        validation: None,
        enforcement: None,
        on_delete: None,
        on_update: None,
    }
}

fn check(name: &str, expression: &str) -> CheckModel {
    CheckModel {
        name: name.to_owned(),
        expression: expression.to_owned(),
        validation: None,
        enforcement: None,
    }
}

fn index(name: &str, columns: &[&str]) -> IndexModel {
    IndexModel {
        name: name.to_owned(),
        columns: columns.iter().map(|column| (*column).to_owned()).collect(),
        expressions: vec![],
        include_columns: vec![],
        unique: false,
        method: None,
        directions: vec![],
        nulls: vec![],
        collations: vec![],
        operator_classes: vec![],
        predicate: None,
    }
}
