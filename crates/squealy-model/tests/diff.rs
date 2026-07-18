use squealy_model::{
    ChangeRisk, CheckModel, ColumnModel, Constraint, DatabaseDiffChange, DatabaseModel,
    DefaultValue, DiffPolicy, EnumModel, ExprNode, ForeignKeyAction, ForeignKeyModel, IndexModel,
    IndexPrefixLength, ProjectionItem, SchemaModel, SourceItem, SourceRef, SqlType,
    TableDiffChange, TableModel, ViewBody, ViewColumnModel, ViewModel, ViewQueryModel,
    check_diff_policy, diff_models,
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

#[test]
fn classifies_safe_database_changes() {
    let desired = model_with_tables("public", vec![table("events")]);
    let actual = DatabaseModel { schemas: vec![] };

    let diff = diff_models(&desired, &actual);

    assert_eq!(
        diff.classified_changes()
            .iter()
            .map(|change| change.risk)
            .collect::<Vec<_>>(),
        vec![ChangeRisk::Safe, ChangeRisk::Safe]
    );
}

#[test]
fn classifies_destructive_database_changes() {
    let desired = DatabaseModel { schemas: vec![] };
    let actual = model_with_tables("public", vec![table("events")]);

    let diff = diff_models(&desired, &actual);

    assert_eq!(
        diff.classified_changes()
            .iter()
            .map(|change| change.risk)
            .collect::<Vec<_>>(),
        vec![ChangeRisk::Destructive, ChangeRisk::Destructive]
    );
}

#[test]
fn classifies_added_columns_by_backfill_safety() {
    let nullable = TableDiffChange::AddColumn {
        column: ColumnModel {
            nullable: true,
            ..column("nickname", SqlType::Text)
        },
    };
    let defaulted = TableDiffChange::AddColumn {
        column: ColumnModel {
            default: Some(DefaultValue::Text("pending".to_owned())),
            ..column("status", SqlType::Text)
        },
    };
    let required = TableDiffChange::AddColumn {
        column: column("name", SqlType::Text),
    };

    assert_eq!(nullable.risk(), ChangeRisk::Safe);
    assert_eq!(defaulted.risk(), ChangeRisk::Safe);
    assert_eq!(required.risk(), ChangeRisk::Ambiguous);
}

#[test]
fn classifies_table_change_by_highest_child_risk() {
    let safe = DatabaseDiffChange::AlterTable {
        schema: Some("public".to_owned()),
        table: "events".to_owned(),
        changes: vec![TableDiffChange::AddIndex {
            index: index("idx_events_name", &["name"]),
        }],
    };
    let ambiguous = DatabaseDiffChange::AlterTable {
        schema: Some("public".to_owned()),
        table: "events".to_owned(),
        changes: vec![TableDiffChange::AlterColumn {
            before: column("name", SqlType::String),
            after: column("name", SqlType::Text),
        }],
    };
    let destructive = DatabaseDiffChange::AlterTable {
        schema: Some("public".to_owned()),
        table: "events".to_owned(),
        changes: vec![
            TableDiffChange::AlterColumn {
                before: column("name", SqlType::String),
                after: column("name", SqlType::Text),
            },
            TableDiffChange::DropColumn {
                column: column("obsolete", SqlType::Text),
            },
        ],
    };

    assert_eq!(safe.risk(), ChangeRisk::Safe);
    assert_eq!(ambiguous.risk(), ChangeRisk::Ambiguous);
    assert_eq!(destructive.risk(), ChangeRisk::Destructive);
}

#[test]
fn default_diff_policy_blocks_ambiguous_and_destructive_changes() {
    let mut desired_events = table("events");
    desired_events.columns = vec![column("name", SqlType::Text)];
    let mut actual_events = table("events");
    actual_events.columns = vec![column("obsolete", SqlType::Text)];
    let diff = diff_models(
        &model_with_tables("public", vec![desired_events]),
        &model_with_tables("public", vec![actual_events]),
    );

    let error = check_diff_policy(&diff, DiffPolicy::default()).unwrap_err();

    assert_eq!(error.blocked.len(), 1);
    assert_eq!(error.blocked[0].risk, ChangeRisk::Destructive);
}

#[test]
fn diff_policy_can_allow_ambiguous_without_allowing_destructive() {
    let mut desired_events = table("events");
    desired_events.columns = vec![column("name", SqlType::Text)];
    let actual_events = table("events");
    let diff = diff_models(
        &model_with_tables("public", vec![desired_events]),
        &model_with_tables("public", vec![actual_events]),
    );

    let policy = DiffPolicy {
        allow_destructive: false,
        allow_ambiguous: true,
    };

    assert!(check_diff_policy(&diff, policy).is_ok());
    assert!(check_diff_policy(&diff, DiffPolicy::default()).is_err());
}

#[test]
fn diff_policy_allows_all_risks_when_requested() {
    let desired = DatabaseModel { schemas: vec![] };
    let actual = model_with_tables("public", vec![table("events")]);
    let diff = diff_models(&desired, &actual);

    assert!(check_diff_policy(&diff, DiffPolicy::ALLOW_ALL).is_ok());
}

#[test]
fn constraint_prefix_length_order_does_not_diff() {
    // Prefix lengths are keyed by column position and render order-independently. Two models whose only
    // difference is the order of a unique constraint's prefix lengths must diff empty (no spurious
    // AlterUnique) — this is the direct `diff_models` path, which never runs `canonicalize_model`.
    let unique = |prefixes: Vec<IndexPrefixLength>| {
        let mut table = table("items");
        table.uniques = vec![Constraint {
            name: "uq_items".to_owned(),
            columns: vec!["a".to_owned(), "b".to_owned()],
            prefix_lengths: prefixes,
        }];
        model_with_tables("public", vec![table])
    };
    let ascending = unique(vec![
        IndexPrefixLength {
            position: 0,
            length: 8,
        },
        IndexPrefixLength {
            position: 1,
            length: 4,
        },
    ]);
    let reversed = unique(vec![
        IndexPrefixLength {
            position: 1,
            length: 4,
        },
        IndexPrefixLength {
            position: 0,
            length: 8,
        },
    ]);

    assert!(
        diff_models(&ascending, &reversed).changes.is_empty(),
        "reordered prefix lengths must not diff: {:?}",
        diff_models(&ascending, &reversed).changes
    );
}

#[test]
fn replacing_a_table_with_a_same_named_enum_drops_the_table_first() {
    // PostgreSQL creates a composite type per table, so an enum named `status` collides with a table
    // `status`. Replacing the table with the enum must drop the table before creating the type.
    let actual = model_with_tables("app", vec![table("status")]);
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "status".to_owned(),
                labels: vec!["open".to_owned(), "closed".to_owned()],
            }],
        }],
    };
    let changes = diff_models(&desired, &actual).changes;
    let drop_at = changes
        .iter()
        .position(
            |c| matches!(c, DatabaseDiffChange::DropTable { table, .. } if table.name == "status"),
        )
        .expect("the table drop must be present");
    let create_at = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateEnum { enum_type, .. } if enum_type.name == "status"))
        .expect("the enum create must be present");
    assert!(
        drop_at < create_at,
        "the same-named table must be dropped before the enum is created: {changes:?}"
    );
}

#[test]
fn replacing_a_table_with_an_enum_used_by_a_new_table_orders_drop_enum_create() {
    // Replace table `status` with enum `status`, and add a new table `t` with a `status`-typed column.
    // Correct order: drop the old `status` table, create the enum, then create `t` that uses it.
    let actual = model_with_tables("app", vec![table("status")]);
    let mut using = table("readings");
    using.columns = vec![column("s", SqlType::Enum("status".to_owned()))];
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![using],
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "status".to_owned(),
                labels: vec!["open".to_owned(), "closed".to_owned()],
            }],
        }],
    };
    let changes = diff_models(&desired, &actual).changes;
    let drop_status = changes
        .iter()
        .position(
            |c| matches!(c, DatabaseDiffChange::DropTable { table, .. } if table.name == "status"),
        )
        .expect("drop of the old status table");
    let create_enum = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateEnum { enum_type, .. } if enum_type.name == "status"))
        .expect("create of the status enum");
    let create_using = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateTable { table, .. } if table.name == "readings"))
        .expect("create of the dependent table");
    assert!(
        drop_status < create_enum && create_enum < create_using,
        "must drop the table, create the enum, then create the dependent table: {changes:?}"
    );
}

#[test]
fn replacing_an_enum_with_a_table_while_dropping_a_dependent_orders_correctly() {
    // Replace enum `status` with a table `status`, while an existing table `u` (which uses the enum) is
    // dropped. Correct order: drop `u`, drop the enum, then create the `status` table.
    let mut dependent = table("u");
    dependent.columns = vec![column("s", SqlType::Enum("status".to_owned()))];
    let actual = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![dependent],
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "status".to_owned(),
                labels: vec!["open".to_owned()],
            }],
        }],
    };
    let desired = model_with_tables("app", vec![table("status")]);
    let changes = diff_models(&desired, &actual).changes;
    let drop_u = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropTable { table, .. } if table.name == "u"))
        .expect("drop of the dependent table u");
    let drop_enum = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropEnum { enum_type, .. } if enum_type.name == "status"))
        .expect("drop of the status enum");
    let create_status = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateTable { table, .. } if table.name == "status"))
        .expect("create of the status table");
    assert!(
        drop_u < drop_enum && drop_enum < create_status,
        "must drop the dependent, drop the enum, then create the table: {changes:?}"
    );
}

/// A minimal view named `name` selecting from a single source relation `source` in schema `app`.
fn view_over(name: &str, source: &str) -> ViewModel {
    ViewModel {
        name: name.to_owned(),
        comment: None,
        columns: Vec::new(),
        query: ViewBody::Select(Box::new(ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "x".to_owned(),
                internal_alias: None,
                expr: ExprNode::Column {
                    alias: "q".to_owned(),
                    column: "x".to_owned(),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: Some("app".to_owned()),
                name: source.to_owned(),
                alias: "q".to_owned(),
            })),
            ..ViewQueryModel::default()
        })),
    }
}

#[test]
fn dropping_a_relation_replaced_by_an_enum_drops_its_dependent_view_first() {
    // Replace table `status` with enum `status`, while a live view `v` selects from the `status` table.
    // Correct order: drop the dependent view `v`, then the `status` table, then create the enum.
    let actual = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![table("status")],
            views: vec![view_over("v", "status")],
            enums: Vec::new(),
        }],
    };
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "status".to_owned(),
                labels: vec!["open".to_owned()],
            }],
        }],
    };
    let changes = diff_models(&desired, &actual).changes;
    let drop_v = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropView { view, .. } if view.name == "v"))
        .expect("drop of the dependent view v");
    let drop_status = changes
        .iter()
        .position(
            |c| matches!(c, DatabaseDiffChange::DropTable { table, .. } if table.name == "status"),
        )
        .expect("drop of the status table");
    let create_enum = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateEnum { enum_type, .. } if enum_type.name == "status"))
        .expect("create of the status enum");
    assert!(
        drop_v < drop_status && drop_status < create_enum,
        "must drop the dependent view, then the table, then create the enum: {changes:?}"
    );
}

#[test]
fn creating_a_relation_replacing_an_enum_creates_its_dependent_view_last() {
    // Replace enum `status` with table `status`, and add a desired view `v` selecting from that table.
    // Correct order: drop the enum, create the `status` table, then create the dependent view `v`.
    let actual = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "status".to_owned(),
                labels: vec!["open".to_owned()],
            }],
        }],
    };
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![table("status")],
            views: vec![view_over("v", "status")],
            enums: Vec::new(),
        }],
    };
    let changes = diff_models(&desired, &actual).changes;
    let drop_enum = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropEnum { enum_type, .. } if enum_type.name == "status"))
        .expect("drop of the status enum");
    let create_status = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateTable { table, .. } if table.name == "status"))
        .expect("create of the status table");
    let create_v = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateView { view, .. } if view.name == "v"))
        .expect("create of the dependent view v");
    assert!(
        drop_enum < create_status && create_status < create_v,
        "must drop the enum, create the table, then create the dependent view: {changes:?}"
    );
}

#[test]
fn dropping_a_relation_replaced_by_an_enum_hoists_a_foreign_key_child_drop() {
    // Replace table `status` with enum `status`, while a child table with a foreign key to `status` is
    // also dropped. The child (whose FK still references `status`) must be dropped before `status`.
    let mut child = table("orders");
    child.foreign_keys = vec![ForeignKeyModel {
        name: "fk_orders_status".to_owned(),
        columns: vec!["status".to_owned()],
        references_schema: Some("app".to_owned()),
        references_table: "status".to_owned(),
        references_columns: vec!["id".to_owned()],
        match_type: None,
        deferrability: None,
        validation: None,
        enforcement: None,
        on_delete: None,
        on_update: None,
    }];
    let actual = model_with_tables("app", vec![table("status"), child]);
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "status".to_owned(),
                labels: vec!["open".to_owned()],
            }],
        }],
    };
    let changes = diff_models(&desired, &actual).changes;
    let drop_child = changes
        .iter()
        .position(
            |c| matches!(c, DatabaseDiffChange::DropTable { table, .. } if table.name == "orders"),
        )
        .expect("drop of the FK-child table");
    let drop_status = changes
        .iter()
        .position(
            |c| matches!(c, DatabaseDiffChange::DropTable { table, .. } if table.name == "status"),
        )
        .expect("drop of the status table");
    assert!(
        drop_child < drop_status,
        "the foreign-key child must be dropped before the referenced table: {changes:?}"
    );
    // The child must not be dropped twice (once hoisted, once in the normal phase).
    assert_eq!(
        changes
            .iter()
            .filter(|c| matches!(c, DatabaseDiffChange::DropTable { table, .. } if table.name == "orders"))
            .count(),
        1,
        "the hoisted child drop must not be duplicated: {changes:?}"
    );
}

#[test]
fn dropping_a_relation_replaced_by_an_enum_drops_a_kept_repointed_view_first() {
    // Replace table `status` with enum `status`; a view `v` that selected from `status` is KEPT but
    // repointed to a `codes` table. The diff only queues a CreateView (replace) for `v`, so a DropView
    // must be materialized before `status` is dropped (its recreate is the deferred CreateView).
    let actual = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![table("status"), table("codes")],
            views: vec![view_over("v", "status")],
            enums: Vec::new(),
        }],
    };
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![table("codes")],
            views: vec![view_over("v", "codes")],
            enums: vec![EnumModel {
                name: "status".to_owned(),
                labels: vec!["open".to_owned()],
            }],
        }],
    };
    let changes = diff_models(&desired, &actual).changes;
    let drop_v = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropView { view, .. } if view.name == "v"))
        .expect("a materialized DropView for the kept, repointed view");
    let drop_status = changes
        .iter()
        .position(
            |c| matches!(c, DatabaseDiffChange::DropTable { table, .. } if table.name == "status"),
        )
        .expect("drop of the status table");
    let create_v = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateView { view, .. } if view.name == "v"))
        .expect("recreate of the repointed view");
    assert!(
        drop_v < drop_status && drop_status < create_v,
        "the kept view is dropped before the table, then recreated after: {changes:?}"
    );
}

fn model_with_tables(schema: &str, tables: Vec<TableModel>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some(schema.to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
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
        on_update: None,
    }
}

fn constraint(name: &str, columns: &[&str]) -> Constraint {
    Constraint {
        prefix_lengths: Vec::new(),
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

fn check_expr(sql: &str) -> ExprNode {
    squealy_parse::Reader::new(squealy_parse::SqlDialect::Generic)
        .read_check_expression(sql)
        .unwrap()
}

fn check(name: &str, expression: &str) -> CheckModel {
    CheckModel {
        name: name.to_owned(),
        expression: check_expr(expression),
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
        prefix_lengths: Vec::new(),
        predicate: None,
    }
}

fn schema_with(name: &str, tables: Vec<TableModel>, views: Vec<ViewModel>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some(name.to_owned()),
            tables,
            views,
            enums: Vec::new(),
        }],
    }
}

// A view named `name` selecting from a single source `from` (a table or another view).
fn dep_view(name: &str, from: &str) -> ViewModel {
    ViewModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "id".to_owned(),
            ty: SqlType::I32,
            nullable: false,
        }],
        query: ViewBody::Select(Box::new(ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                internal_alias: None,
                expr: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "id".to_owned(),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: Some("public".to_owned()),
                name: from.to_owned(),
                alias: "q0_0".to_owned(),
            })),
            joins: Vec::new(),
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })),
    }
}

#[test]
fn diff_creates_dependent_views_after_their_dependencies() {
    // `child` selects from `parent`; both are newly added. The create plan must list `parent` first.
    let desired = schema_with(
        "public",
        vec![table("events")],
        vec![dep_view("child", "parent"), dep_view("parent", "events")],
    );
    let actual = schema_with("public", vec![table("events")], vec![]);

    let changes = diff_models(&desired, &actual).changes;
    let pos = |name: &str| {
        changes
            .iter()
            .position(
                |c| matches!(c, DatabaseDiffChange::CreateView { view, .. } if view.name == name),
            )
            .expect("create present")
    };
    assert!(
        pos("parent") < pos("child"),
        "dependency must be created first: {changes:?}"
    );
}

#[test]
fn diff_drops_dependent_views_before_their_dependencies() {
    // Both views removed; `child` selects from `parent`, so `child` must be dropped first.
    let desired = schema_with("public", vec![table("events")], vec![]);
    let actual = schema_with(
        "public",
        vec![table("events")],
        vec![dep_view("parent", "events"), dep_view("child", "parent")],
    );

    let changes = diff_models(&desired, &actual).changes;
    let pos = |name: &str| {
        changes
            .iter()
            .position(
                |c| matches!(c, DatabaseDiffChange::DropView { view, .. } if view.name == name),
            )
            .expect("drop present")
    };
    assert!(
        pos("child") < pos("parent"),
        "dependent must be dropped first: {changes:?}"
    );
}
