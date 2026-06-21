use squealy::SchemaBackend;
use squealy::*;
use squealy_postgresql::{Postgres, PostgresParam};

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

// Regression (PR #23 review): projecting a nullable column yields `Option<T>`, which requires
// `Option<T>: Decode<Postgres>`. A `#[derive(ColumnType)]` newtype carries `Decode`/`DecodeNullable`
// but not `FromSqlOwned`, so `Option<Newtype>: Decode<Postgres>` only holds once the backend decodes
// `Option<T>` through `T: Decode` (via the row reader's NULL peek) rather than `T: FromSqlOwned`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
struct ProbeId(i32);

// `MIN`/`MAX` support for a newtype is opt-in (it is not derived). `ProbeId` wraps an orderable
// integer, so enable it.
squealy::impl_aggregate_scalar!(ProbeId);

#[test]
fn option_newtype_column_decodes() {
    fn assert_decode<T: squealy::Decode<Postgres>>() {}
    assert_decode::<Option<ProbeId>>();
    assert_decode::<Option<i32>>();
}

/// Every `SUM`/`AVG` output type (and its nullable wrapper, since aggregates are `NULL` over an
/// empty input) must decode on PostgreSQL — otherwise a query that compiles would fail at row
/// decode. `SUM` widens to `i64`/`i128`/`u128` and `AVG` to `f64`; `i128`/`u128` decode from
/// `numeric`, `i64`/`f64` directly.
#[test]
fn sum_and_avg_result_types_decode() {
    fn assert_decode<T: squealy::Decode<Postgres>>() {}
    assert_decode::<Option<i64>>();
    assert_decode::<Option<i128>>();
    assert_decode::<Option<u128>>();
    assert_decode::<Option<f64>>();
}

/// A newtype that opts in via `impl_aggregate_scalar!` supports `MIN`/`MAX` and decodes back to the
/// newtype — including its nullable / left-joined form, which flattens to a single `Option<ProbeId>`
/// rather than `Option<Option<ProbeId>>`.
#[test]
fn newtype_min_max_result_types() {
    let min: <squealy::MinExpr<ProbeId> as squealy::ExprKind>::Value = Some(ProbeId(0));
    let _: Option<ProbeId> = min;

    let nullable_max: <squealy::MaxExpr<squealy::Nullable<ProbeId>> as squealy::ExprKind>::Value =
        Some(ProbeId(0));
    let _: Option<ProbeId> = nullable_max;
}

#[derive(Clone, Debug, PartialEq, Table)]
struct DefaultedRecord<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    count: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    user_id: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
}

#[test]
fn postgres_reports_schema_capabilities() {
    let capabilities = Postgres.capabilities();

    assert!(capabilities.constraints.foreign_key_match_type);
    assert!(capabilities.constraints.foreign_key_deferrability);
    assert!(capabilities.constraints.foreign_key_validation);
    assert!(!capabilities.constraints.foreign_key_enforcement);
    assert!(capabilities.constraints.check_validation);
    assert!(!capabilities.constraints.check_enforcement);
    assert!(capabilities.indexes.predicates);
    assert!(capabilities.indexes.expressions);
    assert!(capabilities.indexes.include_columns);
    assert!(capabilities.indexes.null_ordering);
    assert!(capabilities.indexes.collations);
    assert!(capabilities.indexes.operator_classes);
}

#[test]
fn postgres_renders_incremental_schema_plan() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::CreateSchema {
                schema: Some("public".to_owned()),
            },
            DatabasePlanStep::CreateTable {
                schema: Some("public".to_owned()),
                table: Box::new(TableModel {
                    name: "events".to_owned(),
                    comment: Some("Event records".to_owned()),
                    columns: vec![ColumnModel {
                        name: "id".to_owned(),
                        comment: Some("Event id".to_owned()),
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    }],
                    primary_key: None,
                    foreign_keys: Vec::new(),
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: vec![IndexModel {
                        name: "idx_events_id".to_owned(),
                        columns: vec!["id".to_owned()],
                        expressions: Vec::new(),
                        include_columns: Vec::new(),
                        unique: false,
                        method: None,
                        directions: Vec::new(),
                        nulls: Vec::new(),
                        collations: Vec::new(),
                        operator_classes: Vec::new(),
                        predicate: None,
                    }],
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AddColumn {
                    column: ColumnModel {
                        name: "name".to_owned(),
                        comment: Some("Event name".to_owned()),
                        ty: SqlType::Text,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::DropIndex {
                    index: IndexModel {
                        name: "idx_events_id".to_owned(),
                        columns: vec!["id".to_owned()],
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
            DatabasePlanStep::DropTable {
                schema: Some("public".to_owned()),
                table: Box::new(TableModel {
                    name: "old_events".to_owned(),
                    comment: None,
                    columns: Vec::new(),
                    primary_key: None,
                    foreign_keys: Vec::new(),
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: Vec::new(),
                }),
            },
            DatabasePlanStep::DropSchema {
                schema: Some("old".to_owned()),
            },
        ],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS \"public\";\n\
CREATE TABLE \"public\".\"events\" (\n  \"id\" integer NOT NULL\n);\n\
COMMENT ON TABLE \"public\".\"events\" IS 'Event records';\n\
COMMENT ON COLUMN \"public\".\"events\".\"id\" IS 'Event id';\n\
CREATE INDEX \"idx_events_id\" ON \"public\".\"events\" (\"id\");\n\
ALTER TABLE \"public\".\"events\" ADD COLUMN \"name\" text NOT NULL;\n\
COMMENT ON COLUMN \"public\".\"events\".\"name\" IS 'Event name';\n\
DROP INDEX \"public\".\"idx_events_id\";\n\
DROP TABLE \"public\".\"old_events\";\n\
DROP SCHEMA \"old\";"
    );
}

#[test]
fn postgres_renders_changed_constraints_and_indexes_in_schema_plan() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterPrimaryKey {
                    before: Constraint {
                        name: "pk_events".to_owned(),
                        columns: vec!["id".to_owned()],
                    },
                    after: Constraint {
                        name: "pk_events".to_owned(),
                        columns: vec!["event_id".to_owned()],
                    },
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterUnique {
                    before: Constraint {
                        name: "uq_events_name".to_owned(),
                        columns: vec!["name".to_owned()],
                    },
                    after: Constraint {
                        name: "uq_events_name".to_owned(),
                        columns: vec!["slug".to_owned()],
                    },
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterForeignKey {
                    before: ForeignKeyModel {
                        name: "fk_events_user_id".to_owned(),
                        columns: vec!["user_id".to_owned()],
                        references_schema: Some("public".to_owned()),
                        references_table: "users".to_owned(),
                        references_columns: vec!["id".to_owned()],
                        match_type: None,
                        deferrability: None,
                        validation: None,
                        enforcement: None,
                        on_delete: None,
                        on_update: None,
                    },
                    after: ForeignKeyModel {
                        name: "fk_events_user_id".to_owned(),
                        columns: vec!["owner_id".to_owned()],
                        references_schema: Some("public".to_owned()),
                        references_table: "users".to_owned(),
                        references_columns: vec!["id".to_owned()],
                        match_type: None,
                        deferrability: None,
                        validation: None,
                        enforcement: None,
                        on_delete: Some(ForeignKeyAction::Cascade),
                        on_update: None,
                    },
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterCheck {
                    before: CheckModel {
                        name: "ck_events_id".to_owned(),
                        expression: "id > 0".to_owned(),
                        validation: None,
                        enforcement: None,
                    },
                    after: CheckModel {
                        name: "ck_events_id".to_owned(),
                        expression: "event_id > 0".to_owned(),
                        validation: None,
                        enforcement: None,
                    },
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterIndex {
                    before: IndexModel {
                        name: "idx_events_name".to_owned(),
                        columns: vec!["name".to_owned()],
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
                    after: IndexModel {
                        name: "idx_events_name".to_owned(),
                        columns: vec!["slug".to_owned()],
                        expressions: Vec::new(),
                        include_columns: Vec::new(),
                        unique: true,
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

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "ALTER TABLE \"public\".\"events\" DROP CONSTRAINT \"pk_events\";\n\
ALTER TABLE \"public\".\"events\" ADD CONSTRAINT \"pk_events\" PRIMARY KEY (\"event_id\");\n\
ALTER TABLE \"public\".\"events\" DROP CONSTRAINT \"uq_events_name\";\n\
ALTER TABLE \"public\".\"events\" ADD CONSTRAINT \"uq_events_name\" UNIQUE (\"slug\");\n\
ALTER TABLE \"public\".\"events\" DROP CONSTRAINT \"fk_events_user_id\";\n\
ALTER TABLE \"public\".\"events\" ADD CONSTRAINT \"fk_events_user_id\" FOREIGN KEY (\"owner_id\") REFERENCES \"public\".\"users\" (\"id\") ON DELETE CASCADE;\n\
ALTER TABLE \"public\".\"events\" DROP CONSTRAINT \"ck_events_id\";\n\
ALTER TABLE \"public\".\"events\" ADD CONSTRAINT \"ck_events_id\" CHECK (event_id > 0);\n\
DROP INDEX \"public\".\"idx_events_name\";\n\
CREATE UNIQUE INDEX \"idx_events_name\" ON \"public\".\"events\" (\"slug\");"
    );
}

#[test]
fn postgres_renders_changed_columns_in_schema_plan() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterColumn {
                    type_cast: None,
                    before: ColumnModel {
                        name: "description".to_owned(),
                        comment: Some("Old description".to_owned()),
                        ty: SqlType::String,
                        collation: None,
                        nullable: true,
                        default: Some(DefaultValue::Text("old".to_owned())),
                        identity: None,
                        generated: None,
                    },
                    after: ColumnModel {
                        name: "description".to_owned(),
                        comment: None,
                        ty: SqlType::Varchar(128),
                        collation: Some("C".to_owned()),
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterColumn {
                    type_cast: None,
                    before: ColumnModel {
                        name: "status".to_owned(),
                        comment: None,
                        ty: SqlType::Text,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                    after: ColumnModel {
                        name: "status".to_owned(),
                        comment: Some("Event status".to_owned()),
                        ty: SqlType::Text,
                        collation: None,
                        nullable: true,
                        default: Some(DefaultValue::Text("new".to_owned())),
                        identity: None,
                        generated: None,
                    },
                }),
            },
        ],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "ALTER TABLE \"public\".\"events\" ALTER COLUMN \"description\" TYPE varchar(128) COLLATE \"C\";\n\
ALTER TABLE \"public\".\"events\" ALTER COLUMN \"description\" SET NOT NULL;\n\
ALTER TABLE \"public\".\"events\" ALTER COLUMN \"description\" DROP DEFAULT;\n\
COMMENT ON COLUMN \"public\".\"events\".\"description\" IS NULL;\n\
ALTER TABLE \"public\".\"events\" ALTER COLUMN \"status\" DROP NOT NULL;\n\
ALTER TABLE \"public\".\"events\" ALTER COLUMN \"status\" SET DEFAULT 'new';\n\
COMMENT ON COLUMN \"public\".\"events\".\"status\" IS 'Event status';"
    );
}

#[test]
fn postgres_renders_identity_and_generated_transitions() {
    let plain = |name: &str| ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty: SqlType::I64,
        collation: None,
        nullable: false,
        default: None,
        identity: None,
        generated: None,
    };
    let with_identity = |name: &str, mode: IdentityMode| ColumnModel {
        identity: Some(IdentityModel { mode }),
        ..plain(name)
    };
    let alter = |before: ColumnModel, after: ColumnModel| DatabasePlanStep::AlterTable {
        schema: Some("public".to_owned()),
        table: "events".to_owned(),
        change: Box::new(TablePlanStep::AlterColumn {
            before,
            after,
            type_cast: None,
        }),
    };

    let plan = DatabasePlan {
        steps: vec![
            // Add identity.
            alter(plain("a"), with_identity("a", IdentityMode::Always)),
            // Change identity mode.
            alter(
                with_identity("b", IdentityMode::Always),
                with_identity("b", IdentityMode::ByDefault),
            ),
            // Drop identity.
            alter(with_identity("c", IdentityMode::ByDefault), plain("c")),
            // Drop a generated expression.
            alter(
                ColumnModel {
                    generated: Some(GeneratedColumnModel {
                        expression: "1 + 1".to_owned(),
                        storage: GeneratedStorage::Stored,
                    }),
                    ..plain("d")
                },
                plain("d"),
            ),
        ],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "ALTER TABLE \"public\".\"events\" ALTER COLUMN \"a\" ADD GENERATED ALWAYS AS IDENTITY;\n\
ALTER TABLE \"public\".\"events\" ALTER COLUMN \"b\" SET GENERATED BY DEFAULT;\n\
ALTER TABLE \"public\".\"events\" ALTER COLUMN \"c\" DROP IDENTITY IF EXISTS;\n\
ALTER TABLE \"public\".\"events\" ALTER COLUMN \"d\" DROP EXPRESSION IF EXISTS;"
    );
}

#[test]
fn postgres_rejects_adding_a_generated_column_in_place() {
    let before = ColumnModel {
        name: "total".to_owned(),
        comment: None,
        ty: SqlType::I64,
        collation: None,
        nullable: false,
        default: None,
        identity: None,
        generated: None,
    };
    let after = ColumnModel {
        generated: Some(GeneratedColumnModel {
            expression: "price * qty".to_owned(),
            storage: GeneratedStorage::Stored,
        }),
        ..before.clone()
    };
    let plan = DatabasePlan {
        steps: vec![DatabasePlanStep::AlterTable {
            schema: Some("public".to_owned()),
            table: "orders".to_owned(),
            change: Box::new(TablePlanStep::AlterColumn {
                before,
                after,
                type_cast: None,
            }),
        }],
    };

    let error = Postgres.render_plan(&plan, &mut Vec::new()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn postgres_drops_identity_before_setting_a_default() {
    // Identity and default are mutually exclusive, so DROP IDENTITY must come before SET DEFAULT.
    let before = ColumnModel {
        name: "counter".to_owned(),
        comment: None,
        ty: SqlType::I64,
        collation: None,
        nullable: false,
        default: None,
        identity: Some(IdentityModel {
            mode: IdentityMode::ByDefault,
        }),
        generated: None,
    };
    let after = ColumnModel {
        identity: None,
        default: Some(DefaultValue::Text("ready".to_owned())),
        ..before.clone()
    };
    let plan = DatabasePlan {
        steps: vec![DatabasePlanStep::AlterTable {
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            change: Box::new(TablePlanStep::AlterColumn {
                before,
                after,
                type_cast: None,
            }),
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "ALTER TABLE \"public\".\"events\" ALTER COLUMN \"counter\" DROP IDENTITY IF EXISTS;\n\
ALTER TABLE \"public\".\"events\" ALTER COLUMN \"counter\" SET DEFAULT 'ready';"
    );
}

#[test]
fn postgres_renders_rename_steps_in_schema_plan() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::RenameTable {
                refactor_id: None,
                schema: Some("public".to_owned()),
                from: "app_users".to_owned(),
                to: "users".to_owned(),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "users".to_owned(),
                change: Box::new(TablePlanStep::RenameColumn {
                    refactor_id: None,
                    from: "display_name".to_owned(),
                    to: "name".to_owned(),
                }),
            },
        ],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "ALTER TABLE \"public\".\"app_users\" RENAME TO \"users\";\n\
ALTER TABLE \"public\".\"users\" RENAME COLUMN \"display_name\" TO \"name\";"
    );
}

#[test]
fn postgres_records_refactor_ids_for_rename_steps() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::RenameTable {
                refactor_id: Some("rename-users".to_owned()),
                schema: Some("public".to_owned()),
                from: "app_users".to_owned(),
                to: "users".to_owned(),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "users".to_owned(),
                change: Box::new(TablePlanStep::RenameColumn {
                    refactor_id: Some("rename-display-name".to_owned()),
                    from: "display_name".to_owned(),
                    to: "name".to_owned(),
                }),
            },
        ],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS \"__squealy\";\n\
CREATE TABLE IF NOT EXISTS \"__squealy\".\"refactors\" (\"id\" text PRIMARY KEY, \"applied_at\" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP);\n\
ALTER TABLE \"public\".\"app_users\" RENAME TO \"users\";\n\
INSERT INTO \"__squealy\".\"refactors\" (\"id\") VALUES ('rename-users') ON CONFLICT (\"id\") DO NOTHING;\n\
ALTER TABLE \"public\".\"users\" RENAME COLUMN \"display_name\" TO \"name\";\n\
INSERT INTO \"__squealy\".\"refactors\" (\"id\") VALUES ('rename-display-name') ON CONFLICT (\"id\") DO NOTHING;"
    );
}

#[test]
fn postgres_rejects_unsupported_changed_column_definitions() {
    let mut renamed = column("description");
    renamed.name = "details".to_owned();

    // Adding a generated expression to an existing column is not possible in place on Postgres.
    let mut generated = column("description");
    generated.generated = Some(GeneratedColumnModel {
        expression: "length(description)".to_owned(),
        storage: GeneratedStorage::Stored,
    });

    for after in [renamed, generated] {
        let plan = DatabasePlan {
            steps: vec![DatabasePlanStep::AlterTable {
                schema: Some("public".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterColumn {
                    type_cast: None,
                    before: column("description"),
                    after,
                }),
            }],
        };

        let mut sql = Vec::new();
        let error = Postgres.render_plan(&plan, &mut sql).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
}

fn column(name: &str) -> ColumnModel {
    ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty: SqlType::Text,
        collation: None,
        nullable: true,
        default: None,
        identity: None,
        generated: None,
    }
}

#[test]
fn postgres_select_uses_numbered_placeholders() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.id.equals(1))
        .select(|(user,)| user.id + 2);

    assert_eq!(
        users.to_sql(),
        "SELECT (q0_0.\"id\" + $1) AS \"expr\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" = $2)"
    );
    let mut written = Vec::new();
    users.write_params(&mut written).unwrap();
    assert_eq!(
        written,
        vec![PostgresParam::Int32(2), PostgresParam::Int32(1)]
    );
    assert_eq!(
        users.collect_params().unwrap(),
        vec![PostgresParam::Int32(2), PostgresParam::Int32(1)]
    );
}

#[test]
fn postgres_like_renders_with_numbered_placeholder() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.name.like("A%"))
        .select(|(user,)| user.id);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"name\" LIKE $1)"
    );
    assert_eq!(
        users.collect_params().unwrap(),
        vec![PostgresParam::Text("A%".to_owned())]
    );
}

#[test]
fn postgres_ilike_renders_native_ilike_operator() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.name.ilike("a%"))
        .select(|(user,)| user.id);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"name\" ILIKE $1)"
    );

    let not_ilike = Postgres
        .from::<User>()
        .where_(|user| user.name.not_ilike("a%"))
        .select(|(user,)| user.id);
    assert_eq!(
        not_ilike.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"name\" NOT ILIKE $1)"
    );
}

#[test]
fn postgres_in_renders_numbered_placeholder_list() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.id.in_([1, 2, 3]))
        .select(|(user,)| user.id);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" IN ($1, $2, $3))"
    );
    assert_eq!(
        users.collect_params().unwrap(),
        vec![
            PostgresParam::Int32(1),
            PostgresParam::Int32(2),
            PostgresParam::Int32(3)
        ]
    );
}

#[test]
fn postgres_empty_not_in_with_param_operand_keeps_placeholder_order() {
    // Regression (PR #30 review): an empty IN/NOT IN must still render its operand so the
    // operand's runtime parameter is consumed in order. The NOT IN operand is the first runtime
    // param and the equals is the second, so the equals must bind $2 — not $1.
    let users = Postgres
        .from::<User>()
        .where_(|user| {
            param::<UserId>()
                .not_in(Vec::<i32>::new())
                .and(user.id.equals(param::<UserId>()))
        })
        .select(|(user,)| user.id);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"users\" AS q0_0 \
         WHERE (($1 IS NOT NULL OR 1 = 1) AND (q0_0.\"id\" = $2))"
    );
}

#[test]
fn postgres_between_renders_numbered_placeholders() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.id.between(1, 10))
        .select(|(user,)| user.id);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" BETWEEN $1 AND $2)"
    );
    assert_eq!(
        users.collect_params().unwrap(),
        vec![PostgresParam::Int32(1), PostgresParam::Int32(10)]
    );
}

#[test]
fn postgres_count_and_sum_render_as_aggregates() {
    let q = Postgres
        .from::<User>()
        .select(|(user,)| (user.id.count(), user.id.sum()));
    // `SUM` is cast to `bigint` so the widened `i64` decodes (PostgreSQL `sum(bigint)` is `numeric`).
    assert_eq!(
        q.to_sql(),
        "SELECT COUNT(q0_0.\"id\") AS \"t0_expr\", CAST(SUM(q0_0.\"id\") AS bigint) AS \"t1_expr\" \
         FROM \"public\".\"users\" AS q0_0"
    );
}

#[test]
fn postgres_group_by_having_renders_with_numbered_placeholders() {
    let q = Postgres
        .from::<User>()
        .where_(|user| user.id.greater_than(0))
        .group_by(|(user,)| user.name)
        .having(|(user,)| user.id.count().greater_than(1i64))
        .select(|(user,)| (user.name, user.id.count()));
    assert_eq!(
        q.to_sql(),
        "SELECT q0_0.\"name\" AS \"t0_name\", COUNT(q0_0.\"id\") AS \"t1_expr\" \
         FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" > $1) GROUP BY q0_0.\"name\" \
         HAVING (COUNT(q0_0.\"id\") > $2)"
    );
}

#[test]
fn postgres_avg_min_max_render_as_aggregates() {
    let q = Postgres
        .from::<User>()
        .select(|(user,)| (user.id.avg(), user.id.min(), user.name.max()));
    // `AVG` is cast to `double precision` so the advertised `f64` decodes (PostgreSQL `avg(int)` is
    // `numeric`); `MIN`/`MAX` keep the operand type and need no cast.
    assert_eq!(
        q.to_sql(),
        "SELECT CAST(AVG(q0_0.\"id\") AS double precision) AS \"t0_expr\", \
         MIN(q0_0.\"id\") AS \"t1_expr\", MAX(q0_0.\"name\") AS \"t2_expr\" \
         FROM \"public\".\"users\" AS q0_0"
    );
}

#[test]
fn postgres_division_renders_fractional_result() {
    let users = Postgres.from::<User>().select(|(user,)| user.id / 2);

    assert_eq!(
        users.to_sql(),
        "SELECT (CAST(q0_0.\"id\" AS double precision) / CAST($1 AS double precision)) AS \"expr\" FROM \"public\".\"users\" AS q0_0"
    );
    assert_eq!(
        users.collect_params().unwrap(),
        vec![PostgresParam::Int32(2)]
    );
}

#[test]
fn postgres_runtime_prepared_params_render_without_captured_values() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.name.equals(param::<UserName>()))
        .select(|(user,)| user.name);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"name\" AS \"name\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"name\" = $1)"
    );
    assert_eq!(users.collect_params().unwrap(), Vec::<PostgresParam>::new());
}

#[test]
fn postgres_runtime_prepared_assignment_params_render_without_captured_values() {
    let insert = Postgres
        .to::<User>()
        .name(param::<UserName>())
        .insert_returning(|user| user.id);
    let insert_multiple = Postgres
        .to_columns::<User, (UserName,)>()
        .row((param::<UserName>(),))
        .row((param::<UserName>(),))
        .insert_returning(|user| user.id);
    let update = Postgres
        .to::<User>()
        .name(param::<UserName>())
        .where_(|user| user.id.equals(param::<UserId>()))
        .update_returning(|user| user.name);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1) RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        insert_multiple.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1), ($2) RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE \"public\".\"users\" AS q0_0 SET \"name\" = $1 WHERE (q0_0.\"id\" = $2) RETURNING q0_0.\"name\" AS \"name\""
    );
    assert_eq!(
        insert.collect_params().unwrap(),
        Vec::<PostgresParam>::new()
    );
    assert_eq!(
        insert_multiple.collect_params().unwrap(),
        Vec::<PostgresParam>::new()
    );
    assert_eq!(
        update.collect_params().unwrap(),
        Vec::<PostgresParam>::new()
    );
}

/// A custom type that implements `Encode<Postgres>` (here a derived newtype over `uuid::Uuid`)
/// must be usable as a prepared *runtime* parameter, not only as an inline literal. This exercises
/// the exact bound the prepared-query builder requires — `(UserUuid,)` satisfying
/// `PreparedParamValues<HCons<UserUuid, HNil>, Postgres>` — which relies on the reflexive
/// `IntoPreparedParam<T> for T` impl plus the type's `Encode<Postgres>` impl.
#[cfg(feature = "uuid")]
#[test]
fn postgres_custom_encode_type_is_usable_as_prepared_param() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
    #[column_type(db_type = "uuid")]
    struct UserUuid(uuid::Uuid);

    let id = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    let values = (UserUuid(id),);

    let mut params = Vec::new();
    {
        let mut writer = Postgres::param_writer(&mut params);
        PreparedParamValues::<HCons<UserUuid, HNil>, Postgres>::write_params(&values, &mut writer)
            .expect("encode custom-type prepared param");
    }

    assert_eq!(params, vec![PostgresParam::Uuid(id)]);
}

#[test]
fn postgres_update_renders_explicit_defaults() {
    let update = Postgres
        .to::<User>()
        .name(default())
        .where_(|user| user.id.equals(1))
        .update_returning(|user| user.name);

    assert_eq!(
        update.to_sql(),
        "UPDATE \"public\".\"users\" AS q0_0 SET \"name\" = DEFAULT WHERE (q0_0.\"id\" = $1) RETURNING q0_0.\"name\" AS \"name\""
    );
    assert_eq!(
        update.collect_params().unwrap(),
        vec![PostgresParam::Int32(1)]
    );
}

#[test]
fn postgres_explicit_update_columns_render_expression_assignments() {
    let update = Postgres
        .to_columns::<Counter, (CounterCount,)>()
        .set(|counter| (counter.count + 1,))
        .where_(|counter| counter.id.equals(7))
        .update_returning(|counter| counter.count);

    assert_eq!(
        update.to_sql(),
        "UPDATE \"counters\" AS q0_0 SET \"count\" = (q0_0.\"count\" + $1) WHERE (q0_0.\"id\" = $2) RETURNING q0_0.\"count\" AS \"count\""
    );
    assert_eq!(
        update.collect_params().unwrap(),
        vec![PostgresParam::Int32(1), PostgresParam::Int32(7)]
    );
}

#[test]
fn postgres_source_first_select_renders_from_backend_selected_ast() {
    let users = Postgres
        .from::<User>()
        .order_by(|(user,)| (user.id + 2).desc())
        .where_(|(user,)| user.id.equals(1))
        .limit(10)
        .offset(5)
        .select(|(user,)| user.name);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"name\" AS \"name\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" = $1) ORDER BY (q0_0.\"id\" + $2) DESC LIMIT 10 OFFSET 5"
    );
    assert_eq!(
        users.collect_params().unwrap(),
        vec![PostgresParam::Int32(1), PostgresParam::Int32(2)]
    );
}

#[test]
fn postgres_insert_update_and_delete_render_returning() {
    let insert = Postgres
        .to::<User>()
        .name("Ada")
        .insert_returning(|user| user.id);
    let update = Postgres
        .to::<User>()
        .name("Ada")
        .where_(|user| user.id.equals(1))
        .update_returning(|user| (user.id, user.name));
    let delete = Postgres
        .from::<User>()
        .where_(|user| user.id.equals(1))
        .delete_returning(|user| user);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1) RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE \"public\".\"users\" AS q0_0 SET \"name\" = $1 WHERE (q0_0.\"id\" = $2) RETURNING q0_0.\"id\" AS \"t0_id\", q0_0.\"name\" AS \"t1_name\""
    );
    assert_eq!(
        delete.to_sql(),
        "DELETE FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" = $1) RETURNING q0_0.\"id\" AS \"id\", q0_0.\"name\" AS \"name\""
    );
    assert_eq!(
        insert.collect_params().unwrap(),
        vec![PostgresParam::Text("Ada".to_owned())]
    );
    assert_eq!(
        update.collect_params().unwrap(),
        vec![
            PostgresParam::Text("Ada".to_owned()),
            PostgresParam::Int32(1)
        ]
    );
    assert_eq!(
        delete.collect_params().unwrap(),
        vec![PostgresParam::Int32(1)]
    );
}

#[test]
fn postgres_insert_renders_multiple_rows() {
    let insert = Postgres
        .to_columns::<User, (UserName,)>()
        .row(("Ada",))
        .row(("Grace",))
        .insert_returning(|user| user.id);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1), ($2) RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        insert.collect_params().unwrap(),
        vec![
            PostgresParam::Text("Ada".to_owned()),
            PostgresParam::Text("Grace".to_owned())
        ]
    );
}

#[test]
fn postgres_insert_renders_explicit_defaults() {
    let insert = Postgres
        .to_columns::<User, (UserName,)>()
        .row((default(),))
        .row(("Grace",))
        .insert_returning(|user| user.id + 1);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES (DEFAULT), ($1) RETURNING (\"id\" + $2) AS \"expr\""
    );
    assert_eq!(
        insert.collect_params().unwrap(),
        vec![
            PostgresParam::Text("Grace".to_owned()),
            PostgresParam::Int32(1)
        ]
    );
}

#[test]
fn postgres_insert_can_use_default_values() {
    let insert = Postgres
        .to::<DefaultedRecord>()
        .insert_returning(|record| record.id);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"defaulted_records\" DEFAULT VALUES RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        insert.collect_params().unwrap(),
        Vec::<PostgresParam>::new()
    );
}

#[test]
fn postgres_mutation_returning_expressions_continue_placeholder_numbering() {
    let insert = Postgres
        .to::<User>()
        .name("Ada")
        .insert_returning(|user| user.id + 1);
    let update = Postgres
        .to::<User>()
        .name("Ada")
        .where_(|user| user.id.equals(1))
        .update_returning(|user| user.id + 2);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1) RETURNING (\"id\" + $2) AS \"expr\""
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE \"public\".\"users\" AS q0_0 SET \"name\" = $1 WHERE (q0_0.\"id\" = $2) RETURNING (q0_0.\"id\" + $3) AS \"expr\""
    );
    assert_eq!(
        insert.collect_params().unwrap(),
        vec![
            PostgresParam::Text("Ada".to_owned()),
            PostgresParam::Int32(1)
        ]
    );
    assert_eq!(
        update.collect_params().unwrap(),
        vec![
            PostgresParam::Text("Ada".to_owned()),
            PostgresParam::Int32(1),
            PostgresParam::Int32(2),
        ]
    );
}

#[test]
fn postgres_backend_writes_table_ddl() {
    let mut sql = Vec::new();
    let tables = <Public as Schema>::tables().collect::<Vec<_>>();
    Postgres.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE TABLE \"public\".\"users\" (\"id\" integer PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY NOT NULL, \"name\" text NOT NULL)"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Vault)]
struct Secret<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    ciphertext: C::Type<'scope, Vec<u8>>,
    wrapped_dek: C::Type<'scope, Option<Vec<u8>>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Vault {
    secrets: Secret<'static, ColumnName>,
}

#[test]
fn postgres_writes_bytea_column_ddl() {
    let mut sql = Vec::new();
    let tables = <Vault as Schema>::tables().collect::<Vec<_>>();
    Postgres.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // A `Vec<u8>` column renders as non-null `bytea`; the `Option<Vec<u8>>` column as nullable `bytea`.
    assert!(sql.contains("\"ciphertext\" bytea NOT NULL"), "{sql}");
    assert!(sql.contains("\"wrapped_dek\" bytea"), "{sql}");
    assert!(
        !sql.contains("\"wrapped_dek\" bytea NOT NULL"),
        "nullable bytea must not be NOT NULL: {sql}"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

// Reserved words, defaults, nullable, foreign keys, and multiple unnamed indexes
// all exercise the DDL identifier-quoting and index-naming paths.
#[derive(Clone, Debug, PartialEq, Table)]
#[index(columns = [email])]
#[index(columns = [order, select])]
struct Member<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,

    #[column(references(Account::id, on_delete = "cascade"))]
    account_id: C::Type<'scope, i32>,

    // `order` is a reserved word; it must be quoted to produce valid DDL.
    order: C::Type<'scope, i32>,

    select: C::Type<'scope, Option<i32>>,

    #[column(default = value("anonymous"))]
    email: C::Type<'scope, String>,
}

fn member_metadata() -> Member<'static, ColumnName> {
    <Member<'static> as SchemaTable>::column_names()
}

fn render_ddl(table: &(dyn Table + Sync)) -> String {
    let mut sql = Vec::new();
    Postgres.write_table(table, &mut sql).unwrap();
    String::from_utf8(sql).unwrap()
}

#[test]
fn postgres_ddl_quotes_reserved_word_identifiers() {
    let table = member_metadata();
    let sql = render_ddl(&table);

    // Reserved-word column names are quoted so the DDL stays valid.
    assert!(
        sql.contains("\"order\" integer NOT NULL"),
        "reserved-word column not quoted: {sql}"
    );
    assert!(
        sql.contains("\"select\" integer"),
        "nullable reserved-word column missing: {sql}"
    );
    // The nullable column has no NOT NULL constraint.
    assert!(
        !sql.contains("\"select\" integer NOT NULL"),
        "nullable column should not be NOT NULL: {sql}"
    );
}

#[test]
fn postgres_ddl_renders_foreign_key_and_default() {
    let table = member_metadata();
    let sql = render_ddl(&table);

    assert!(
        sql.contains(
            "\"account_id\" integer NOT NULL REFERENCES \"accounts\"(\"id\") ON DELETE cascade"
        ),
        "foreign key not rendered as expected: {sql}"
    );
    assert!(
        sql.contains("\"email\" text NOT NULL DEFAULT 'anonymous'"),
        "default literal not rendered as expected: {sql}"
    );
}

#[test]
fn postgres_ddl_gives_unnamed_indexes_distinct_names() {
    let table = member_metadata();
    let sql = render_ddl(&table);

    // Each unnamed index gets a deterministic, distinct name derived from its columns.
    assert!(
        sql.contains("CREATE INDEX \"idx_members_email\" ON \"members\" (\"email\")"),
        "first unnamed index missing or wrong: {sql}"
    );
    assert!(
        sql.contains(
            "CREATE INDEX \"idx_members_order_select\" ON \"members\" (\"order\", \"select\")"
        ),
        "second unnamed index missing or wrong: {sql}"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Accented<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(name = "café")]
    cafe: C::Type<'scope, String>,
}

#[test]
fn postgres_renders_non_ascii_identifiers() {
    // The string-backed SQL writer validates each write chunk as UTF-8, so quoting
    // must emit whole characters rather than individual bytes. Rendering a multibyte
    // identifier through to_sql() would otherwise panic mid-character.
    let query = Postgres
        .from::<Accented>()
        .select(|(row,)| (row.id, row.cafe));

    assert_eq!(
        query.to_sql(),
        "SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"café\" AS \"t1_café\" FROM \"accenteds\" AS q0_0"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Tenant<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    slug: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Membership<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(index, references(Tenant::id, on_delete = "cascade"))]
    tenant_id: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Catalog {
    tenants: Tenant<'static, ColumnName>,
    memberships: Membership<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct CatalogDb {
    catalog: Catalog,
}

#[test]
fn postgres_renders_create_from_scratch() {
    let model = DatabaseModel::from_database::<CatalogDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // Phases: namespace, tables (with inline PK/unique), indexes, then FKs as ALTER ADD CONSTRAINT.
    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS \"catalog\";\n\
CREATE TABLE \"catalog\".\"tenants\" (\n  \"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n  \"slug\" text NOT NULL,\n  CONSTRAINT \"pk_tenants\" PRIMARY KEY (\"id\"),\n  CONSTRAINT \"uq_tenants_slug\" UNIQUE (\"slug\")\n);\n\
CREATE TABLE \"catalog\".\"memberships\" (\n  \"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n  \"tenant_id\" integer NOT NULL,\n  CONSTRAINT \"pk_memberships\" PRIMARY KEY (\"id\")\n);\n\
CREATE INDEX \"idx_memberships_tenant_id\" ON \"catalog\".\"memberships\" (\"tenant_id\");\n\
ALTER TABLE \"catalog\".\"memberships\" ADD CONSTRAINT \"fk_memberships_tenant_id\" FOREIGN KEY (\"tenant_id\") REFERENCES \"catalog\".\"tenants\" (\"id\") ON DELETE CASCADE;"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
#[primary_key(columns = [tenant_id, id])]
struct Seat<'scope, C: ColumnMode = ColumnExpr> {
    tenant_id: C::Type<'scope, i32>,
    id: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct SeatCatalog {
    seats: Seat<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct SeatDb {
    catalog: SeatCatalog,
}

#[test]
fn postgres_renders_compound_primary_key() {
    let model = DatabaseModel::from_database::<SeatDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("CONSTRAINT \"pk_seats\" PRIMARY KEY (\"tenant_id\", \"id\")"),
        "expected compound PRIMARY KEY in: {sql}"
    );
}

#[cfg(feature = "systemtime")]
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(AuditCatalog)]
struct Audit<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    created_at: C::Type<'scope, std::time::SystemTime>,
    deleted_at: C::Type<'scope, Option<std::time::SystemTime>>,
}

#[cfg(feature = "systemtime")]
#[allow(dead_code)]
#[derive(Schema)]
struct AuditCatalog {
    audits: Audit<'static, ColumnName>,
}

#[cfg(feature = "systemtime")]
#[allow(dead_code)]
#[derive(Database)]
struct AuditDb {
    catalog: AuditCatalog,
}

#[cfg(feature = "systemtime")]
#[test]
fn postgres_renders_timestamp_columns_from_systemtime_fields() {
    let model = DatabaseModel::from_database::<AuditDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // A bare `SystemTime` field maps to a timezone-aware timestamp column with no `db_type` override,
    // nullable or not.
    assert!(
        sql.contains("\"created_at\" timestamp with time zone NOT NULL"),
        "expected non-null timestamptz column in: {sql}"
    );
    // The nullable column renders the same type with no `NOT NULL`.
    assert!(
        sql.contains("\"deleted_at\" timestamp with time zone,"),
        "expected nullable timestamptz column in: {sql}"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
#[unique(columns = [organization_id, slug])]
struct Repository<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    organization_id: C::Type<'scope, i32>,
    slug: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct RepositoryCatalog {
    repositorys: Repository<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct RepositoryDb {
    catalog: RepositoryCatalog,
}

#[test]
fn postgres_renders_composite_unique_constraint() {
    let model = DatabaseModel::from_database::<RepositoryDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CONSTRAINT \"uq_repositorys_organization_id_slug\" UNIQUE (\"organization_id\", \"slug\")"
        ),
        "expected composite UNIQUE constraint in: {sql}"
    );
}

#[test]
fn postgres_backend_writes_composite_unique_ddl() {
    // The query-side single-table `write_table` path must also emit table-level `#[unique(..)]`
    // constraints, otherwise duplicates are allowed even though `render_create` forbids them.
    let mut sql = Vec::new();
    let tables = <RepositoryCatalog as Schema>::tables().collect::<Vec<_>>();
    Postgres.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("UNIQUE (\"organization_id\", \"slug\")"),
        "expected composite UNIQUE constraint in write_table output: {sql}"
    );
}

#[test]
fn postgres_backend_writes_compound_primary_key_ddl() {
    // The query-side single-table `write_table` path must also honor a table-level primary key
    // (no column carries `primary_key()` in this case).
    let mut sql = Vec::new();
    let tables = <SeatCatalog as Schema>::tables().collect::<Vec<_>>();
    Postgres.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE TABLE \"catalog\".\"seats\" (\"tenant_id\" integer NOT NULL, \"id\" integer NOT NULL, PRIMARY KEY (\"tenant_id\", \"id\"))"
    );
}

// A soft-delete table: `slug` is unique only among live rows (`deleted_at IS NULL`), so a slug is
// reusable once a row is soft-deleted. `external_id` carries a plain, unconditional unique.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Organization<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    #[column(unique, where = |row| row.deleted_at.is_null())]
    slug: C::Type<'scope, String>,
    #[column(unique)]
    external_id: C::Type<'scope, i32>,
    deleted_at: C::Type<'scope, Option<i64>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct OrganizationCatalog {
    organizations: Organization<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct OrganizationDb {
    catalog: OrganizationCatalog,
}

// Composite, table-level partial unique: `(organization_id, slug)` unique among live rows only.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
#[unique(columns = [organization_id, slug], where = |row| row.deleted_at.is_null())]
struct Workspace<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    organization_id: C::Type<'scope, i32>,
    slug: C::Type<'scope, String>,
    deleted_at: C::Type<'scope, Option<i64>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct WorkspaceCatalog {
    workspaces: Workspace<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct WorkspaceDb {
    catalog: WorkspaceCatalog,
}

#[test]
fn postgres_renders_partial_unique_index_for_soft_delete() {
    let model = DatabaseModel::from_database::<OrganizationDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // The predicated `#[column(unique, where = ...)]` becomes a partial UNIQUE INDEX, keeping the
    // `uq_<table>_<column>` identity but rendering the `WHERE` from the typed predicate.
    assert!(
        sql.contains(
            "CREATE UNIQUE INDEX \"uq_organizations_slug\" ON \"organization_catalog\".\"organizations\" (\"slug\") WHERE (\"deleted_at\" IS NULL)"
        ),
        "expected partial unique index in: {sql}"
    );
    // A plain `#[column(unique)]` still renders as an unconditional constraint.
    assert!(
        sql.contains("UNIQUE (\"external_id\")"),
        "expected plain unique constraint in: {sql}"
    );
    // The predicated column must NOT also produce an unconditional UNIQUE over `slug`.
    assert!(
        !sql.contains("UNIQUE (\"slug\")"),
        "slug must not carry an unconditional unique constraint: {sql}"
    );
}

#[test]
fn postgres_renders_composite_partial_unique_index() {
    let model = DatabaseModel::from_database::<WorkspaceDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE UNIQUE INDEX \"uq_workspaces_organization_id_slug\" ON \"workspace_catalog\".\"workspaces\" (\"organization_id\", \"slug\") WHERE (\"deleted_at\" IS NULL)"
        ),
        "expected composite partial unique index in: {sql}"
    );
    assert!(
        !sql.contains("UNIQUE (\"organization_id\", \"slug\")"),
        "composite predicated unique must not render as a table constraint: {sql}"
    );
}

#[test]
fn postgres_write_table_emits_partial_unique_index() {
    // The query-side `write_table` path must honor the predicate too: emit a partial unique index
    // rather than silently dropping or over-constraining with an unconditional `UNIQUE`.
    let mut sql = Vec::new();
    let tables = <WorkspaceCatalog as Schema>::tables().collect::<Vec<_>>();
    Postgres.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("CREATE UNIQUE INDEX \"uq_workspaces_organization_id_slug\"")
            && sql.contains("WHERE (\"deleted_at\" IS NULL)"),
        "expected partial unique index in write_table output: {sql}"
    );
    assert!(
        !sql.contains("UNIQUE (\"organization_id\", \"slug\")"),
        "predicated unique must not render as an inline constraint: {sql}"
    );
}

#[test]
fn postgres_write_table_emits_column_level_partial_unique_index() {
    // The column form `#[column(unique, where = ...)]` lives on `Column::unique_predicate()`, not
    // `table.uniques()`; the direct `write_table` path must still emit its partial index so
    // soft-delete uniqueness is enforced in the query-side create flow.
    let mut sql = Vec::new();
    let tables = <OrganizationCatalog as Schema>::tables().collect::<Vec<_>>();
    Postgres.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE UNIQUE INDEX \"uq_organizations_slug\" ON \"catalog\".\"organizations\" (\"slug\") WHERE (\"deleted_at\" IS NULL)"
        ),
        "expected column-level partial unique index in write_table output: {sql}"
    );
    // The plain `#[column(unique)]` external_id is unaffected; slug carries no inline UNIQUE.
    assert!(
        !sql.contains("UNIQUE (\"slug\")"),
        "slug must not render as an inline unique: {sql}"
    );
}

// A predicate combining a NULL check with a scalar value-literal comparison: `email` is unique
// among live, active rows only.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Roster<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    #[column(unique, where = |row| row.deleted_at.is_null().and(row.status.equals(1)))]
    email: C::Type<'scope, String>,
    status: C::Type<'scope, i32>,
    deleted_at: C::Type<'scope, Option<i64>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct RosterCatalog {
    rosters: Roster<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct RosterDb {
    catalog: RosterCatalog,
}

// A predicate comparing a column to a string literal that contains a single quote, to exercise
// SQL string-literal escaping in the DDL predicate renderer.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Region<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    #[column(unique, where = |row| row.label.equals("o'brien"))]
    code: C::Type<'scope, String>,
    label: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct RegionCatalog {
    regions: Region<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct RegionDb {
    catalog: RegionCatalog,
}

#[test]
fn postgres_renders_partial_unique_index_with_literal_predicate() {
    let model = DatabaseModel::from_database::<RosterDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // A scalar value literal renders inline (no bind placeholder), alongside the NULL check.
    assert!(
        sql.contains(
            "CREATE UNIQUE INDEX \"uq_rosters_email\" ON \"roster_catalog\".\"rosters\" (\"email\") WHERE ((\"deleted_at\" IS NULL) AND (\"status\" = 1))"
        ),
        "expected partial unique index with inline literal in: {sql}"
    );
}

#[test]
fn postgres_escapes_string_literal_in_partial_index_predicate() {
    let model = DatabaseModel::from_database::<RegionDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // The embedded single quote is doubled, matching backend text-literal quoting.
    assert!(
        sql.contains("WHERE (\"label\" = 'o''brien')"),
        "expected escaped string literal in predicate: {sql}"
    );
}

#[test]
fn postgres_renders_table_and_column_comments() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            views: Vec::new(),
            tables: vec![TableModel {
                name: "tenants".to_owned(),
                comment: Some("Tenant records".to_owned()),
                columns: vec![ColumnModel {
                    name: "slug".to_owned(),
                    comment: Some("Tenant's stable slug".to_owned()),
                    ty: SqlType::String,
                    collation: Some("C".to_owned()),
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS \"catalog\";\n\
CREATE TABLE \"catalog\".\"tenants\" (\n  \"slug\" text COLLATE \"C\" NOT NULL\n);\n\
COMMENT ON TABLE \"catalog\".\"tenants\" IS 'Tenant records';\n\
COMMENT ON COLUMN \"catalog\".\"tenants\".\"slug\" IS 'Tenant''s stable slug';"
    );
}

#[test]
fn postgres_renders_foreign_key_match_type() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            views: Vec::new(),
            tables: vec![
                TableModel {
                    name: "memberships".to_owned(),
                    comment: None,
                    columns: vec![ColumnModel {
                        name: "tenant_id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    }],
                    primary_key: None,
                    foreign_keys: vec![ForeignKeyModel {
                        name: "fk_memberships_tenant_id".to_owned(),
                        columns: vec!["tenant_id".to_owned()],
                        references_schema: Some("catalog".to_owned()),
                        references_table: "tenants".to_owned(),
                        references_columns: vec!["id".to_owned()],
                        match_type: Some(ForeignKeyMatch::Full),
                        deferrability: Some(ConstraintDeferrability::InitiallyDeferred),
                        validation: Some(ConstraintValidation::NotValidated),
                        enforcement: None,
                        on_delete: Some(ForeignKeyAction::Cascade),
                        on_update: None,
                    }],
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: Vec::new(),
                },
                TableModel {
                    name: "tenants".to_owned(),
                    comment: None,
                    columns: vec![ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    }],
                    primary_key: None,
                    foreign_keys: Vec::new(),
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: Vec::new(),
                },
            ],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "ALTER TABLE \"catalog\".\"memberships\" ADD CONSTRAINT \"fk_memberships_tenant_id\" FOREIGN KEY (\"tenant_id\") REFERENCES \"catalog\".\"tenants\" (\"id\") MATCH FULL ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED"
        ),
        "foreign key match type not rendered as expected: {sql}"
    );
    assert!(
        sql.contains(" DEFERRABLE INITIALLY DEFERRED NOT VALID"),
        "foreign key validation not rendered as expected: {sql}"
    );
}

#[test]
fn postgres_renders_partial_indexes() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            views: Vec::new(),
            tables: vec![TableModel {
                name: "memberships".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "tenant_id".to_owned(),
                    comment: None,
                    ty: SqlType::I32,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_memberships_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Desc],
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: Some("(tenant_id > 0)".to_owned()),
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE INDEX \"idx_memberships_tenant_id\" ON \"catalog\".\"memberships\" USING btree (\"tenant_id\" DESC) WHERE (tenant_id > 0)"
        ),
        "partial index not rendered as expected: {sql}"
    );
}

#[test]
fn postgres_renders_expression_indexes() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            views: Vec::new(),
            tables: vec![TableModel {
                name: "tenants".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "slug".to_owned(),
                    comment: None,
                    ty: SqlType::String,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_tenants_lower_slug".to_owned(),
                    columns: Vec::new(),
                    expressions: vec!["lower(slug)".to_owned()],
                    include_columns: Vec::new(),
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Asc],
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: None,
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE INDEX \"idx_tenants_lower_slug\" ON \"catalog\".\"tenants\" USING btree (lower(slug) ASC)"
        ),
        "expression index not rendered as expected: {sql}"
    );
}

#[test]
fn postgres_renders_covering_indexes() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            views: Vec::new(),
            tables: vec![TableModel {
                name: "memberships".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "tenant_id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                    ColumnModel {
                        name: "role_code".to_owned(),
                        comment: None,
                        ty: SqlType::String,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                ],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_memberships_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                    expressions: Vec::new(),
                    include_columns: vec!["role_code".to_owned()],
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Asc],
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: None,
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE INDEX \"idx_memberships_tenant_id\" ON \"catalog\".\"memberships\" USING btree (\"tenant_id\" ASC) INCLUDE (\"role_code\")"
        ),
        "covering index not rendered as expected: {sql}"
    );
}

#[test]
fn postgres_renders_index_null_ordering() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            views: Vec::new(),
            tables: vec![TableModel {
                name: "memberships".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "tenant_id".to_owned(),
                    comment: None,
                    ty: SqlType::I32,
                    collation: None,
                    nullable: true,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_memberships_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Asc],
                    nulls: vec![IndexNullsOrder::First],
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: None,
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE INDEX \"idx_memberships_tenant_id\" ON \"catalog\".\"memberships\" USING btree (\"tenant_id\" ASC NULLS FIRST)"
        ),
        "index null ordering not rendered as expected: {sql}"
    );
}

#[test]
fn postgres_renders_index_operator_classes() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            views: Vec::new(),
            tables: vec![TableModel {
                name: "tenants".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "slug".to_owned(),
                    comment: None,
                    ty: SqlType::String,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_tenants_slug_pattern".to_owned(),
                    columns: vec!["slug".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Asc],
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: vec![IndexOperatorClass {
                        position: 0,
                        name: "text_pattern_ops".to_owned(),
                    }],
                    predicate: None,
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE INDEX \"idx_tenants_slug_pattern\" ON \"catalog\".\"tenants\" USING btree (\"slug\" text_pattern_ops ASC)"
        ),
        "index operator class not rendered as expected: {sql}"
    );
}

#[test]
fn postgres_renders_index_collations() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            views: Vec::new(),
            tables: vec![TableModel {
                name: "tenants".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "slug".to_owned(),
                    comment: None,
                    ty: SqlType::String,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_tenants_slug_pattern".to_owned(),
                    columns: vec!["slug".to_owned()],
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Asc],
                    nulls: Vec::new(),
                    collations: vec![IndexCollation {
                        position: 0,
                        name: "C".to_owned(),
                    }],
                    operator_classes: vec![IndexOperatorClass {
                        position: 0,
                        name: "text_pattern_ops".to_owned(),
                    }],
                    predicate: None,
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE INDEX \"idx_tenants_slug_pattern\" ON \"catalog\".\"tenants\" USING btree (\"slug\" COLLATE \"C\" text_pattern_ops ASC)"
        ),
        "index collation not rendered as expected: {sql}"
    );
}

/// Builds a single-column `id` table optionally referencing `references_table` via a foreign key, for
/// the multi-table create-ordering test below.
fn fk_test_table(name: &str, references_table: Option<&str>) -> Box<TableModel> {
    let foreign_keys = references_table
        .map(|target| ForeignKeyModel {
            name: format!("fk_{name}_{target}"),
            columns: vec!["id".to_owned()],
            references_schema: Some("public".to_owned()),
            references_table: target.to_owned(),
            references_columns: vec!["id".to_owned()],
            match_type: None,
            deferrability: None,
            validation: None,
            enforcement: None,
            on_delete: None,
            on_update: None,
        })
        .into_iter()
        .collect();
    Box::new(TableModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![ColumnModel {
            name: "id".to_owned(),
            comment: None,
            ty: SqlType::I32,
            collation: None,
            nullable: false,
            default: None,
            identity: None,
            generated: None,
        }],
        primary_key: None,
        foreign_keys,
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
    })
}

#[test]
fn postgres_defers_foreign_keys_until_all_tables_are_created() {
    // `comments` is created first but references `posts`, created second. The foreign key must be
    // deferred until after both `CREATE TABLE`s, or it would reference a table that does not exist yet.
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::CreateTable {
                schema: Some("public".to_owned()),
                table: fk_test_table("comments", Some("posts")),
            },
            DatabasePlanStep::CreateTable {
                schema: Some("public".to_owned()),
                table: fk_test_table("posts", None),
            },
        ],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    let comments_create = sql.find("CREATE TABLE \"public\".\"comments\"").unwrap();
    let posts_create = sql.find("CREATE TABLE \"public\".\"posts\"").unwrap();
    let fk = sql.find("ADD CONSTRAINT \"fk_comments_posts\"").unwrap();
    assert!(
        comments_create < posts_create && posts_create < fk,
        "foreign key not deferred until after both tables were created: {sql}"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Subscriber<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    nickname: C::Type<'scope, Option<String>>,
}

#[test]
fn postgres_is_null_renders_is_null() {
    let subscribers = Postgres
        .from::<Subscriber>()
        .where_(|subscriber| subscriber.nickname.is_null())
        .select(|(subscriber,)| subscriber.id);

    assert_eq!(
        subscribers.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"subscribers\" AS q0_0 WHERE (q0_0.\"nickname\" IS NULL)"
    );
    assert_eq!(
        subscribers.collect_params().unwrap(),
        Vec::<PostgresParam>::new()
    );
}

#[test]
fn postgres_is_not_null_composes_with_other_predicates() {
    let subscribers = Postgres
        .from::<Subscriber>()
        .where_(|subscriber| {
            subscriber
                .nickname
                .is_not_null()
                .or(subscriber.id.equals(1))
        })
        .select(|(subscriber,)| subscriber.id);

    assert_eq!(
        subscribers.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"subscribers\" AS q0_0 WHERE ((q0_0.\"nickname\" IS NOT NULL) OR (q0_0.\"id\" = $1))"
    );
    assert_eq!(
        subscribers.collect_params().unwrap(),
        vec![PostgresParam::Int32(1)]
    );
}

// View rendering: a view's structural body becomes `CREATE VIEW … AS SELECT …`, emitted after all
// tables, and views are ordered so a view that selects from another view is created after it.
#[test]
fn postgres_renders_views_in_dependency_order() {
    // `projection` is `(output_name, column)`; each expression is `q0_0.<column>`.
    fn view(
        name: &str,
        from: &str,
        projection: &[(&str, &str)],
        filter: Option<ExprNode>,
    ) -> ViewModel {
        ViewModel {
            name: name.to_owned(),
            comment: None,
            columns: projection
                .iter()
                .map(|(output, _)| ViewColumnModel {
                    name: (*output).to_owned(),
                    ty: SqlType::I32,
                    nullable: false,
                })
                .collect(),
            query: ViewQueryModel {
                distinct: false,
                projection: projection
                    .iter()
                    .map(|(output, column)| ProjectionItem {
                        output_name: (*output).to_owned(),
                        expr: ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: (*column).to_owned(),
                        },
                    })
                    .collect(),
                from: Some(SourceRef {
                    schema: Some("public".to_owned()),
                    name: from.to_owned(),
                    alias: "q0_0".to_owned(),
                }),
                joins: Vec::new(),
                filter,
                group_by: Vec::new(),
                having: None,
                order_by: Vec::new(),
                limit: None,
                offset: None,
            },
        }
    }

    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: vec![TableModel {
                name: "users".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "id".to_owned(),
                    comment: None,
                    ty: SqlType::I32,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
            // `active_user_ids` selects from the `active_users` view, so it must be created after it
            // even though it is declared first.
            views: vec![
                view("active_user_ids", "active_users", &[("id", "id")], None),
                view(
                    "active_users",
                    "users",
                    &[("id", "id")],
                    Some(ExprNode::Compare {
                        op: CompareOp::GreaterThan,
                        left: Box::new(ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "id".to_owned(),
                        }),
                        right: Box::new(ExprNode::Literal("0".to_owned())),
                    }),
                ),
            ],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE VIEW \"public\".\"active_users\" (\"id\") AS \
SELECT q0_0.\"id\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" > 0)"
        ),
        "missing active_users view: {sql}"
    );
    assert!(
        sql.contains(
            "CREATE VIEW \"public\".\"active_user_ids\" (\"id\") AS \
SELECT q0_0.\"id\" FROM \"public\".\"active_users\" AS q0_0"
        ),
        "missing active_user_ids view: {sql}"
    );

    let table_pos = sql.find("CREATE TABLE").unwrap();
    let active_users_pos = sql.find("\"active_users\" (").unwrap();
    let active_ids_pos = sql.find("\"active_user_ids\" (").unwrap();
    assert!(
        table_pos < active_users_pos,
        "tables must precede views: {sql}"
    );
    assert!(
        active_users_pos < active_ids_pos,
        "a view must be created after the view it depends on: {sql}"
    );
}

// Incremental plan rendering: a CreateView step renders CREATE OR REPLACE VIEW (so a body change
// re-runs cleanly) and a DropView step renders DROP VIEW.
#[test]
fn postgres_renders_view_plan_steps() {
    let view = ViewModel {
        name: "active_users".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "id".to_owned(),
            ty: SqlType::I32,
            nullable: false,
        }],
        query: ViewQueryModel {
            distinct: false,
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                expr: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "id".to_owned(),
                },
            }],
            from: Some(SourceRef {
                schema: Some("public".to_owned()),
                name: "users".to_owned(),
                alias: "q0_0".to_owned(),
            }),
            joins: Vec::new(),
            filter: Some(ExprNode::Compare {
                op: CompareOp::GreaterThan,
                left: Box::new(ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "id".to_owned(),
                }),
                right: Box::new(ExprNode::Literal("0".to_owned())),
            }),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        },
    };

    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::CreateView {
                schema: Some("public".to_owned()),
                view: Box::new(view.clone()),
            },
            DatabasePlanStep::DropView {
                schema: Some("public".to_owned()),
                view: Box::new(view),
            },
        ],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE OR REPLACE VIEW \"public\".\"active_users\" (\"id\") AS \
SELECT q0_0.\"id\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" > 0)"
        ),
        "missing create-or-replace: {sql}"
    );
    assert!(
        sql.contains("DROP VIEW \"public\".\"active_users\""),
        "missing drop view: {sql}"
    );
}

#[test]
fn postgres_renders_distinct_view_body() {
    let view = ViewModel {
        name: "distinct_names".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "name".to_owned(),
            ty: SqlType::String,
            nullable: false,
        }],
        query: ViewQueryModel {
            distinct: true,
            projection: vec![ProjectionItem {
                output_name: "name".to_owned(),
                expr: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "name".to_owned(),
                },
            }],
            from: Some(SourceRef {
                schema: Some("public".to_owned()),
                name: "users".to_owned(),
                alias: "q0_0".to_owned(),
            }),
            joins: Vec::new(),
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        },
    };

    let plan = DatabasePlan {
        steps: vec![DatabasePlanStep::CreateView {
            schema: Some("public".to_owned()),
            view: Box::new(view),
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE OR REPLACE VIEW \"public\".\"distinct_names\" (\"name\") AS \
SELECT DISTINCT q0_0.\"name\" FROM \"public\".\"users\" AS q0_0"
        ),
        "distinct view body not rendered: {sql}"
    );
}

#[test]
fn postgres_in_subquery_numbers_placeholders_across_the_nesting_boundary() {
    // The subquery's literal ($1) and the outer filter's literal ($2) must number continuously
    // across the nested SELECT — the whole point of sharing one `Renderer` between parent and child.
    let q = Postgres
        .from::<User>()
        .where_correlated(|(user,), sub| {
            user.id.in_subquery(
                sub.from::<Post>()
                    .where_(|post| post.user_id.equals(7))
                    .select_subquery(|(post,)| post.user_id),
            )
        })
        .where_(|(user,)| user.id.equals(1))
        .select(|(user,)| user.id);

    assert_eq!(
        q.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" IN \
         (SELECT q1_0.\"user_id\" AS \"user_id\" FROM \"public\".\"posts\" AS q1_0 WHERE \
         (q1_0.\"user_id\" = $1))) AND (q0_0.\"id\" = $2)"
    );
    assert_eq!(
        q.collect_params().unwrap(),
        vec![PostgresParam::Int32(7), PostgresParam::Int32(1)]
    );
}

#[test]
fn postgres_exists_correlated_subquery_renders() {
    let q = Postgres
        .from::<User>()
        .where_correlated(|(user,), sub| {
            exists(
                sub.from::<Post>()
                    .where_(|post| post.user_id.equals(user.id))
                    .select_subquery(|(post,)| post.user_id),
            )
        })
        .select(|(user,)| user.id);

    assert_eq!(
        q.to_sql(),
        "SELECT q0_0.\"id\" AS \"id\" FROM \"public\".\"users\" AS q0_0 WHERE (EXISTS \
         (SELECT q1_0.\"user_id\" AS \"user_id\" FROM \"public\".\"posts\" AS q1_0 WHERE \
         (q1_0.\"user_id\" = q0_0.\"id\")))"
    );
}

// The structural expression IR is rendered per-dialect: PostgreSQL keeps the fractional-division
// float casts and precise integer cast types that a single canonical fragment could not express.
#[test]
fn postgres_renders_view_expression_ir_in_its_dialect() {
    fn col(c: &str) -> ExprNode {
        ExprNode::Column {
            alias: "q0_0".to_owned(),
            column: c.to_owned(),
        }
    }
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: Vec::new(),
            views: vec![ViewModel {
                name: "metrics".to_owned(),
                comment: None,
                columns: vec![
                    ViewColumnModel {
                        name: "ratio".to_owned(),
                        ty: SqlType::F64,
                        nullable: false,
                    },
                    ViewColumnModel {
                        name: "total".to_owned(),
                        ty: SqlType::I64,
                        nullable: false,
                    },
                ],
                query: ViewQueryModel {
                    distinct: false,
                    projection: vec![
                        // count / 2 — fractional division.
                        ProjectionItem {
                            output_name: "ratio".to_owned(),
                            expr: ExprNode::Binary {
                                op: ArithmeticOp::Divide,
                                left: Box::new(col("count")),
                                right: Box::new(ExprNode::Literal("2".to_owned())),
                            },
                        },
                        // SUM(amount) cast to i64 so the column wire type matches.
                        ProjectionItem {
                            output_name: "total".to_owned(),
                            expr: ExprNode::Aggregate {
                                func: AggregateFunc::Sum,
                                distinct: false,
                                operand: Box::new(col("amount")),
                                result: Some(SqlType::I64),
                            },
                        },
                    ],
                    from: Some(SourceRef {
                        schema: Some("public".to_owned()),
                        name: "events".to_owned(),
                        alias: "q0_0".to_owned(),
                    }),
                    joins: Vec::new(),
                    filter: None,
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                },
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // Fractional division: operands cast to double precision (no silent integer truncation).
    assert!(
        sql.contains("(CAST(q0_0.\"count\" AS double precision) / CAST(2 AS double precision))"),
        "division float-cast missing: {sql}"
    );
    // Aggregate result cast uses the precise integer type name, not `numeric`.
    assert!(
        sql.contains("CAST(SUM(q0_0.\"amount\") AS bigint)"),
        "aggregate integer cast missing: {sql}"
    );
}

#[test]
fn postgres_window_functions_render_with_numbered_placeholders() {
    let row_num = Postgres.from::<Post>().select(|(post,)| {
        row_number().over(|w| w.partition_by(post.user_id).order_by(post.id.asc()))
    });
    assert_eq!(
        row_num.to_sql(),
        "SELECT ROW_NUMBER() OVER (PARTITION BY q0_0.\"user_id\" ORDER BY q0_0.\"id\" ASC) \
         AS \"expr\" FROM \"public\".\"posts\" AS q0_0"
    );

    // NTILE and LAG bind their integer arguments, numbered continuously ($1, $2).
    let args = Postgres.from::<Post>().select(|(post,)| {
        (
            ntile(4).over(|w| w.order_by(post.id.asc())),
            lag(post.user_id, 2).over(|w| w.order_by(post.id.asc())),
        )
    });
    let sql = args.to_sql();
    assert!(sql.contains("NTILE($1)"), "{sql}");
    assert!(sql.contains("LAG(q0_0.\"user_id\", $2)"), "{sql}");
    assert_eq!(
        args.collect_params().unwrap(),
        vec![PostgresParam::Int32(4), PostgresParam::Int32(2)]
    );

    // Aggregate-over wraps the cast around the whole window expression, not the inner aggregate.
    let summed = Postgres
        .from::<Post>()
        .select(|(post,)| post.user_id.sum().over(|w| w.partition_by(post.user_id)));
    let summed_sql = summed.to_sql();
    assert!(
        summed_sql.contains("SUM(q0_0.\"user_id\") OVER (PARTITION BY q0_0.\"user_id\")"),
        "{summed_sql}"
    );
}

// PostgreSQL supports `NULLS FIRST`/`NULLS LAST`, so a view body carrying an explicit null ordering
// renders it (the counterpart to MySQL dropping it).
#[test]
fn postgres_view_order_by_keeps_nulls_modifier() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: Vec::new(),
            views: vec![ViewModel {
                name: "ranked".to_owned(),
                comment: None,
                columns: vec![ViewColumnModel {
                    name: "id".to_owned(),
                    ty: SqlType::I32,
                    nullable: true,
                }],
                query: ViewQueryModel {
                    distinct: false,
                    projection: vec![ProjectionItem {
                        output_name: "id".to_owned(),
                        expr: ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "id".to_owned(),
                        },
                    }],
                    from: Some(SourceRef {
                        schema: Some("public".to_owned()),
                        name: "events".to_owned(),
                        alias: "q0_0".to_owned(),
                    }),
                    joins: Vec::new(),
                    filter: None,
                    group_by: Vec::new(),
                    having: None,
                    order_by: vec![OrderItem {
                        expr: ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "id".to_owned(),
                        },
                        direction: Some(OrderDirection::Desc),
                        nulls: Some(OrderNulls::Last),
                    }],
                    limit: None,
                    offset: None,
                },
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("ORDER BY q0_0.\"id\" DESC NULLS LAST"),
        "expected NULLS LAST in PG view: {sql}"
    );
}

// An introspected view carries an empty body (its definition can't be reconstructed). Rendering DDL
// from such a model is misuse and must fail clearly rather than emit `AS SELECT` with no projection.
#[test]
fn postgres_render_rejects_empty_view_body() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: Vec::new(),
            views: vec![ViewModel {
                name: "broken".to_owned(),
                comment: None,
                columns: vec![ViewColumnModel {
                    name: "id".to_owned(),
                    ty: SqlType::I32,
                    nullable: false,
                }],
                query: ViewQueryModel::default(),
            }],
        }],
    };

    let mut sql = Vec::new();
    let result = Postgres.render_create(&model, &mut sql);
    assert!(
        result.is_err(),
        "rendering an introspected empty-body view must fail, got: {}",
        String::from_utf8_lossy(&sql)
    );
}

#[test]
fn postgres_distinct_renders_after_select() {
    let users = Postgres
        .from::<User>()
        .distinct()
        .select(|(user,)| user.name);
    assert_eq!(
        users.to_sql(),
        "SELECT DISTINCT q0_0.\"name\" AS \"name\" FROM \"public\".\"users\" AS q0_0"
    );
}

#[test]
fn postgres_distinct_leaves_numbered_placeholders_unaffected() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.id.equals(1))
        .distinct()
        .select(|(user,)| user.name);
    assert_eq!(
        users.to_sql(),
        "SELECT DISTINCT q0_0.\"name\" AS \"name\" FROM \"public\".\"users\" AS q0_0 \
         WHERE (q0_0.\"id\" = $1)"
    );
    assert_eq!(
        users.collect_params().unwrap(),
        vec![PostgresParam::Int32(1)]
    );
}

#[test]
fn postgres_count_distinct_renders_distinct_inside_call() {
    let q = Postgres
        .from::<User>()
        .select(|(user,)| user.id.count().distinct());
    assert_eq!(
        q.to_sql(),
        "SELECT COUNT(DISTINCT q0_0.\"id\") AS \"expr\" FROM \"public\".\"users\" AS q0_0"
    );
}

#[test]
fn postgres_sum_distinct_keeps_cast_around_distinct_call() {
    let q = Postgres
        .from::<User>()
        .select(|(user,)| user.id.sum().distinct());
    assert_eq!(
        q.to_sql(),
        "SELECT CAST(SUM(DISTINCT q0_0.\"id\") AS bigint) AS \"expr\" \
         FROM \"public\".\"users\" AS q0_0"
    );
}

// Pin a query's decoded row type at compile time (a nullability regression fails the bound).
fn assert_join_row<'b, 's, Base, Projection, Q, R>(_q: &Q)
where
    Q: squealy::SelectQuery<'b, 's, Base, Projection, Row = R>,
    Base: squealy::SelectAst<'b, 's, Q::Builder>,
    Projection: squealy::Projectable,
{
}

#[test]
fn postgres_right_join_renders_right_join() {
    let q = Postgres
        .from::<User>()
        .right_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| (user.id, post.id));
    assert_eq!(
        q.to_sql(),
        "SELECT q0_0.\"id\" AS \"t0_id\", q0_1.\"id\" AS \"t1_id\" FROM \"public\".\"users\" AS q0_0 \
         RIGHT JOIN \"public\".\"posts\" AS q0_1 ON (q0_1.\"user_id\" = q0_0.\"id\")"
    );
    // Base (`user.id`) becomes nullable; the joined table (`post.id`) stays non-null.
    assert_join_row::<_, _, _, (Option<i32>, i32)>(&q);
}

#[test]
fn postgres_full_join_renders_full_join() {
    let q = Postgres
        .from::<User>()
        .full_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| (user.id, post.id));
    assert_eq!(
        q.to_sql(),
        "SELECT q0_0.\"id\" AS \"t0_id\", q0_1.\"id\" AS \"t1_id\" FROM \"public\".\"users\" AS q0_0 \
         FULL JOIN \"public\".\"posts\" AS q0_1 ON (q0_1.\"user_id\" = q0_0.\"id\")"
    );
    // Both sides nullable.
    assert_join_row::<_, _, _, (Option<i32>, Option<i32>)>(&q);
}
