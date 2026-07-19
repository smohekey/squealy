use squealy_model::DatabaseDiff;
use squealy_model::DomainModel;
use squealy_model::{
    ChangeRisk, CheckModel, ColumnModel, Constraint, DatabaseDiffChange, DatabaseModel,
    DefaultValue, DiffPolicy, EnumModel, ExclusionElement, ExclusionModel, ExclusionTerm, ExprNode,
    ForeignKeyAction, ForeignKeyModel, IndexMethod, IndexModel, IndexPrefixLength, ProjectionItem,
    SchemaModel, SequenceDataType, SequenceModel, SequenceOwnedBy, SourceItem, SourceRef, SqlType,
    TableDiffChange, TableModel, ViewBody, ViewColumnModel, ViewModel, ViewQueryModel,
    check_diff_policy, diff_models, reject_enum_relation_collision_in_diff,
    reject_enum_relation_name_collision,
};

fn range_exclusion(name: &str) -> ExclusionModel {
    ExclusionModel {
        name: name.to_owned(),
        method: Some(IndexMethod::Gist),
        elements: vec![ExclusionElement {
            term: ExclusionTerm::Column("during".to_owned()),
            operator: "&&".to_owned(),
            operator_class: None,
            collation: None,
            direction: None,
            nulls: None,
        }],
        predicate: None,
        deferrability: None,
    }
}

fn plain_index(name: &str) -> IndexModel {
    IndexModel {
        name: name.to_owned(),
        columns: vec!["id".to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    }
}

#[test]
fn an_exclusion_and_a_same_named_index_swap_is_rejected() {
    // An exclusion owns a `pg_class` index, so replacing it with a plain index of the same name in one
    // migration cannot be ordered (the new index is created before the old exclusion's index is dropped).
    // The guard must reject it up front rather than emit a plan PostgreSQL aborts with "already exists".
    let mut actual_table = table("events");
    actual_table.exclusions = vec![range_exclusion("evt_key")];
    let actual = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![actual_table],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    let mut desired_table = table("events");
    desired_table.indexes = vec![plain_index("evt_key")];
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![desired_table],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    let error = reject_enum_relation_name_collision(&desired, &actual)
        .expect_err("an exclusion↔same-name-index swap must be rejected");
    assert_eq!(error.name, "evt_key");
}

#[test]
fn an_exclusion_and_a_same_named_enum_are_accepted() {
    // An exclusion is backed only by an index (no `pg_type`), so — like a plain index — it does not
    // collide with an enum of the same name. The guard must not over-reject this valid pairing.
    let mut excluded = table("events");
    excluded.exclusions = vec![range_exclusion("mood")];
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![excluded],
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "mood".to_owned(),
                labels: vec!["ok".to_owned()],
            }],
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    assert!(
        reject_enum_relation_name_collision(&desired, &DatabaseModel::default()).is_ok(),
        "an exclusion and a same-named enum must be accepted"
    );
}

#[test]
fn reordered_domain_checks_do_not_diff() {
    let domain = |checks: Vec<CheckModel>| DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: vec![DomainModel {
                name: "d".to_owned(),
                base_type: SqlType::I32,
                not_null: false,
                default: None,
                checks,
            }],
        }],
    };
    let a = CheckModel {
        name: "a_check".to_owned(),
        expression: ExprNode::DomainValue,
        validation: None,
        enforcement: None,
    };
    let z = CheckModel {
        name: "z_check".to_owned(),
        expression: ExprNode::Literal("TRUE".to_owned()),
        validation: None,
        enforcement: None,
    };
    // The same named checks in opposite declaration order must not diff.
    let changes = diff_models(&domain(vec![a.clone(), z.clone()]), &domain(vec![z, a])).changes;
    assert!(
        changes.is_empty(),
        "reordered domain checks must not diff: {changes:?}"
    );
}

#[test]
fn dropping_a_domain_orders_it_before_dropping_its_enum_base() {
    // A domain based on an enum (carried as a `Raw` base) depends on the enum, so `DROP DOMAIN` must
    // precede `DROP TYPE` when both are removed.
    let actual = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "mood".to_owned(),
                labels: vec!["ok".to_owned()],
            }],
            sequences: Vec::new(),
            domains: vec![DomainModel {
                name: "feeling".to_owned(),
                base_type: SqlType::Raw("app.mood".to_owned()),
                not_null: false,
                default: None,
                checks: Vec::new(),
            }],
        }],
    };
    let changes = diff_models(&DatabaseModel::default(), &actual).changes;
    let drop_domain = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropDomain { .. }))
        .expect("a DropDomain");
    let drop_enum = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropEnum { .. }))
        .expect("a DropEnum");
    assert!(
        drop_domain < drop_enum,
        "the domain must drop before its enum base: {changes:?}"
    );
}

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

fn enum_only(schema: &str, name: &str) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some(schema.to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: vec![EnumModel {
                name: name.to_owned(),
                labels: vec!["open".to_owned()],
            }],
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    }
}

#[test]
fn replacing_a_table_with_a_same_named_enum_is_rejected() {
    // PostgreSQL creates a composite type per table, so an enum named `status` collides with a table
    // `status`. Correctly ordering that swap plus its arbitrary dependents is deferred, so the plan
    // path rejects the collision up front rather than emitting an un-applyable diff.
    let actual = model_with_tables("app", vec![table("status")]);
    let desired = enum_only("app", "status");
    let error = reject_enum_relation_name_collision(&desired, &actual)
        .expect_err("a table replaced by a same-named enum must be rejected");
    assert_eq!(error.schema.as_deref(), Some("app"));
    assert_eq!(error.name, "status");
}

#[test]
fn replacing_an_enum_with_a_same_named_relation_is_rejected() {
    // The collision is symmetric: an enum on the actual side and a relation on the desired side is
    // rejected the same way as the reverse.
    let actual = enum_only("app", "status");
    let desired = model_with_tables("app", vec![table("status")]);
    let error = reject_enum_relation_name_collision(&desired, &actual)
        .expect_err("an enum replaced by a same-named relation must be rejected");
    assert_eq!(error.name, "status");
}

#[test]
fn a_relation_and_an_enum_with_distinct_names_are_accepted() {
    let actual = model_with_tables("app", vec![table("status")]);
    let desired = enum_only("app", "mood");
    assert!(reject_enum_relation_name_collision(&desired, &actual).is_ok());
}

fn bigint_sequence(name: &str, owned_by: Option<SequenceOwnedBy>) -> SequenceModel {
    SequenceModel {
        name: name.to_owned(),
        data_type: SequenceDataType::BigInt,
        start: 1,
        increment: 1,
        min: 1,
        max: i64::MAX,
        cache: 1,
        cycle: false,
        owned_by,
    }
}

#[test]
fn creating_an_owned_sequence_orders_create_before_table_and_owner_after() {
    // A new sequence owned by a new table: the bare `CreateSequence` must precede the `CreateTable` (a
    // column could `nextval` it), and the `SetSequenceOwner` must follow it (the owning column must
    // exist).
    let actual = DatabaseModel::default();
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![table("events")],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: vec![bigint_sequence(
                "events_id_seq",
                Some(SequenceOwnedBy {
                    table: "events".to_owned(),
                    column: "id".to_owned(),
                }),
            )],
            domains: Vec::new(),
        }],
    };
    let changes = diff_models(&desired, &actual).changes;
    let create_seq = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateSequence { .. }))
        .expect("a CreateSequence");
    let create_table = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::CreateTable { .. }))
        .expect("a CreateTable");
    let set_owner = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::SetSequenceOwner { .. }))
        .expect("a SetSequenceOwner");
    assert!(
        create_seq < create_table && create_table < set_owner,
        "sequence created before table, owner set after: {changes:?}"
    );
}

#[test]
fn a_sequence_sharing_a_table_name_is_rejected() {
    // A sequence and a table both live in PostgreSQL's per-schema pg_class namespace, so they cannot
    // share a name; the collision guard must reject it (here, within a single desired model).
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![table("counter")],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: vec![bigint_sequence("counter", None)],
            domains: Vec::new(),
        }],
    };
    let error = reject_enum_relation_name_collision(&desired, &DatabaseModel::default())
        .expect_err("a sequence sharing a table name must be rejected");
    assert_eq!(error.name, "counter");
}

fn named_index(name: &str) -> IndexModel {
    IndexModel {
        name: name.to_owned(),
        columns: vec!["id".to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    }
}

#[test]
fn precomputed_diff_rejects_a_sequence_colliding_with_an_altered_index() {
    // The precomputed-diff guard must claim index names from `AlterIndex` (not only add/drop), so a
    // caller cannot hand `plan_diff` a diff that alters index `counter` while creating a sequence
    // `counter` — a plan PostgreSQL rejects over the shared pg_class namespace.
    let diff = DatabaseDiff {
        changes: vec![
            DatabaseDiffChange::AlterTable {
                schema: Some("app".to_owned()),
                table: "events".to_owned(),
                changes: vec![TableDiffChange::AlterIndex {
                    before: named_index("counter"),
                    after: named_index("counter"),
                }],
            },
            DatabaseDiffChange::CreateSequence {
                schema: Some("app".to_owned()),
                sequence: bigint_sequence("counter", None),
            },
        ],
    };
    let error = reject_enum_relation_collision_in_diff(&diff)
        .expect_err("a sequence colliding with an altered index must be rejected");
    assert_eq!(error.name, "counter");
}

#[test]
fn a_sequence_sharing_an_index_name_is_rejected() {
    // A sequence and an index both occupy PostgreSQL's per-schema pg_class namespace (verified on PG 17),
    // so they cannot share a name.
    let mut indexed = table("events");
    indexed.indexes = vec![IndexModel {
        name: "counter".to_owned(),
        columns: vec!["id".to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    }];
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![indexed],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: vec![bigint_sequence("counter", None)],
            domains: Vec::new(),
        }],
    };
    let error = reject_enum_relation_name_collision(&desired, &DatabaseModel::default())
        .expect_err("a sequence sharing an index name must be rejected");
    assert_eq!(error.name, "counter");
}

#[test]
fn a_sequence_sharing_an_enum_name_is_rejected() {
    // A sequence owns an associated pg_type, which collides with an enum of the same name (verified: PG 17
    // reports "a relation has an associated type of the same name").
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "mood".to_owned(),
                labels: vec!["ok".to_owned()],
            }],
            sequences: vec![bigint_sequence("mood", None)],
            domains: Vec::new(),
        }],
    };
    let error = reject_enum_relation_name_collision(&desired, &DatabaseModel::default())
        .expect_err("a sequence sharing an enum name must be rejected");
    assert_eq!(error.name, "mood");
}

#[test]
fn an_enum_and_a_same_named_index_are_accepted() {
    // An index has no associated pg_type, so it does *not* collide with an enum of the same name (verified
    // on PG 17). The guard must not over-reject this valid pairing.
    let mut indexed = table("events");
    indexed.indexes = vec![IndexModel {
        name: "mood".to_owned(),
        columns: vec!["id".to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    }];
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![indexed],
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "mood".to_owned(),
                labels: vec!["ok".to_owned()],
            }],
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    assert!(
        reject_enum_relation_name_collision(&desired, &DatabaseModel::default()).is_ok(),
        "an enum and a same-named index must be accepted"
    );
}

#[test]
fn dropping_an_owned_sequence_detaches_it_before_the_table_drop() {
    // A sequence OWNED BY a table being dropped would be cascade-dropped by PostgreSQL, making the
    // explicit DropSequence fail. The diff must detach it (SetSequenceOwner -> NONE) before the table
    // work and drop it after.
    let owned = |()| DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![table("events")],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: vec![bigint_sequence(
                "events_id_seq",
                Some(SequenceOwnedBy {
                    table: "events".to_owned(),
                    column: "id".to_owned(),
                }),
            )],
            domains: Vec::new(),
        }],
    };
    let actual = owned(());
    let desired = DatabaseModel::default();
    let changes = diff_models(&desired, &actual).changes;
    let detach = changes
        .iter()
        .position(|c| {
            matches!(c, DatabaseDiffChange::SetSequenceOwner { owned_by, .. } if owned_by.is_none())
        })
        .expect("a detach SetSequenceOwner -> NONE");
    let drop_table = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropTable { .. }))
        .expect("a DropTable");
    let drop_seq = changes
        .iter()
        .position(|c| matches!(c, DatabaseDiffChange::DropSequence { .. }))
        .expect("a DropSequence");
    assert!(
        detach < drop_table && drop_table < drop_seq,
        "detach before the table drop, drop the sequence after: {changes:?}"
    );
}

#[test]
fn an_unchanged_sequence_produces_no_diff() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: vec![bigint_sequence("s", None)],
            domains: Vec::new(),
        }],
    };
    assert!(
        diff_models(&model, &model).changes.is_empty(),
        "an identical sequence must not diff"
    );
}

#[test]
fn changing_a_sequence_attribute_is_an_alter_not_a_recreate() {
    let base = |increment: i64| DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: vec![SequenceModel {
                increment,
                ..bigint_sequence("s", None)
            }],
            domains: Vec::new(),
        }],
    };
    let changes = diff_models(&base(2), &base(1)).changes;
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, DatabaseDiffChange::AlterSequence { .. })),
        "an attribute change is an AlterSequence: {changes:?}"
    );
    assert!(
        !changes
            .iter()
            .any(|c| matches!(c, DatabaseDiffChange::DropSequence { .. })),
        "an attribute change must not drop the sequence: {changes:?}"
    );
}

fn model_with_tables(schema: &str, tables: Vec<TableModel>) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some(schema.to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
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
        exclusions: Vec::new(),
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
            sequences: Vec::new(),
            domains: Vec::new(),
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
        materialized: false,
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
