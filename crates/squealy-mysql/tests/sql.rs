use squealy::*;
use squealy_mysql::Mysql;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
struct Tenant<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    slug: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
struct Membership<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    // Matches the referenced `Tenant::id` (signed `i32`): MySQL requires FK integer columns to agree
    // in size and sign. `seats` below still exercises unsigned-type rendering.
    #[column(index, references(Tenant::id, on_delete = "cascade"))]
    tenant_id: C::Type<'scope, i32>,
    seats: C::Type<'scope, u16>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Shop {
    tenants: Tenant<'static, ColumnName>,
    memberships: Membership<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct ShopDb {
    shop: Shop,
}

#[test]
fn mysql_reports_schema_capabilities() {
    let capabilities = Mysql.capabilities();

    assert!(!capabilities.constraints.foreign_key_match_type);
    assert!(!capabilities.constraints.foreign_key_deferrability);
    assert!(!capabilities.constraints.foreign_key_validation);
    assert!(!capabilities.constraints.foreign_key_enforcement);
    assert!(!capabilities.constraints.check_validation);
    assert!(!capabilities.constraints.check_enforcement);
    assert!(!capabilities.indexes.predicates);
    assert!(!capabilities.indexes.expressions);
    assert!(!capabilities.indexes.include_columns);
    assert!(!capabilities.indexes.null_ordering);
    assert!(!capabilities.indexes.collations);
    assert!(!capabilities.indexes.operator_classes);
}

#[test]
fn mysql_renders_incremental_schema_plan() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::CreateSchema {
                schema: Some("shop".to_owned()),
            },
            DatabasePlanStep::CreateTable {
                schema: Some("shop".to_owned()),
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
                schema: Some("shop".to_owned()),
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
                schema: Some("shop".to_owned()),
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
                schema: Some("shop".to_owned()),
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
    Mysql.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS `shop`;\n\
CREATE TABLE `shop`.`events` (\n  `id` INT NOT NULL COMMENT 'Event id'\n) COMMENT='Event records';\n\
CREATE INDEX `idx_events_id` ON `shop`.`events` (`id`);\n\
ALTER TABLE `shop`.`events` ADD COLUMN `name` TEXT NOT NULL COMMENT 'Event name';\n\
DROP INDEX `idx_events_id` ON `shop`.`events`;\n\
DROP TABLE `shop`.`old_events`;\n\
DROP SCHEMA `old`;"
    );
}

#[test]
fn mysql_renders_changed_constraints_and_indexes_in_schema_plan() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
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
                schema: Some("shop".to_owned()),
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
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterForeignKey {
                    before: ForeignKeyModel {
                        name: "fk_events_user_id".to_owned(),
                        columns: vec!["user_id".to_owned()],
                        references_schema: Some("shop".to_owned()),
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
                        references_schema: Some("shop".to_owned()),
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
                schema: Some("shop".to_owned()),
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
                schema: Some("shop".to_owned()),
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
    Mysql.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "ALTER TABLE `shop`.`events` DROP PRIMARY KEY;\n\
ALTER TABLE `shop`.`events` ADD CONSTRAINT `pk_events` PRIMARY KEY (`event_id`);\n\
ALTER TABLE `shop`.`events` DROP INDEX `uq_events_name`;\n\
ALTER TABLE `shop`.`events` ADD CONSTRAINT `uq_events_name` UNIQUE (`slug`);\n\
ALTER TABLE `shop`.`events` DROP FOREIGN KEY `fk_events_user_id`;\n\
ALTER TABLE `shop`.`events` ADD CONSTRAINT `fk_events_user_id` FOREIGN KEY (`owner_id`) REFERENCES `shop`.`users` (`id`) ON DELETE CASCADE;\n\
ALTER TABLE `shop`.`events` DROP CHECK `ck_events_id`;\n\
ALTER TABLE `shop`.`events` ADD CONSTRAINT `ck_events_id` CHECK (event_id > 0);\n\
DROP INDEX `idx_events_name` ON `shop`.`events`;\n\
CREATE UNIQUE INDEX `idx_events_name` ON `shop`.`events` (`slug`);"
    );
}

#[test]
fn mysql_renders_changed_columns_in_schema_plan() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterColumn {
                    type_cast: None,
                    before: ColumnModel {
                        name: "description".to_owned(),
                        comment: Some("Old description".to_owned()),
                        ty: SqlType::String,
                        collation: None,
                        nullable: true,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                    after: ColumnModel {
                        name: "description".to_owned(),
                        comment: Some("New description".to_owned()),
                        ty: SqlType::Varchar(128),
                        collation: Some("utf8mb4_bin".to_owned()),
                        nullable: false,
                        default: Some(DefaultValue::Text("new".to_owned())),
                        identity: None,
                        generated: None,
                    },
                }),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: Box::new(TablePlanStep::AlterColumn {
                    type_cast: None,
                    before: ColumnModel {
                        name: "status".to_owned(),
                        comment: Some("Event status".to_owned()),
                        ty: SqlType::Text,
                        collation: None,
                        nullable: false,
                        default: Some(DefaultValue::Text("draft".to_owned())),
                        identity: None,
                        generated: None,
                    },
                    after: ColumnModel {
                        name: "status".to_owned(),
                        comment: None,
                        ty: SqlType::Text,
                        collation: None,
                        nullable: true,
                        default: None,
                        identity: None,
                        generated: None,
                    },
                }),
            },
        ],
    };

    let mut sql = Vec::new();
    Mysql.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "ALTER TABLE `shop`.`events` MODIFY COLUMN `description` VARCHAR(128) COLLATE utf8mb4_bin NOT NULL DEFAULT 'new' COMMENT 'New description';\n\
ALTER TABLE `shop`.`events` MODIFY COLUMN `status` TEXT;"
    );
}

#[test]
fn mysql_renders_rename_steps_in_schema_plan() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::RenameTable {
                refactor_id: None,
                schema: Some("shop".to_owned()),
                from: "app_users".to_owned(),
                to: "users".to_owned(),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "users".to_owned(),
                change: Box::new(TablePlanStep::RenameColumn {
                    refactor_id: None,
                    from: "display_name".to_owned(),
                    to: "name".to_owned(),
                    column_type: SqlType::String,
                }),
            },
        ],
    };

    let mut sql = Vec::new();
    Mysql.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "RENAME TABLE `shop`.`app_users` TO `shop`.`users`;\n\
ALTER TABLE `shop`.`users` RENAME COLUMN `display_name` TO `name`;"
    );
}

#[test]
fn mysql_records_refactor_ids_for_rename_steps() {
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::RenameTable {
                refactor_id: Some("rename-users".to_owned()),
                schema: Some("shop".to_owned()),
                from: "app_users".to_owned(),
                to: "users".to_owned(),
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "users".to_owned(),
                change: Box::new(TablePlanStep::RenameColumn {
                    refactor_id: Some("rename-display-name".to_owned()),
                    from: "display_name".to_owned(),
                    to: "name".to_owned(),
                    column_type: SqlType::String,
                }),
            },
        ],
    };

    let mut sql = Vec::new();
    Mysql.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS `__squealy`;\n\
CREATE TABLE IF NOT EXISTS `__squealy`.`refactors` (`id` VARCHAR(255) NOT NULL PRIMARY KEY, `applied_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP);\n\
RENAME TABLE `shop`.`app_users` TO `shop`.`users`;\n\
INSERT IGNORE INTO `__squealy`.`refactors` (`id`) VALUES ('rename-users');\n\
ALTER TABLE `shop`.`users` RENAME COLUMN `display_name` TO `name`;\n\
INSERT IGNORE INTO `__squealy`.`refactors` (`id`) VALUES ('rename-display-name');"
    );
}

#[test]
fn mysql_rejects_unsupported_changed_column_definitions() {
    // Column rename is still expressed via explicit refactor steps, not an in-place `MODIFY`.
    let mut renamed = column("description");
    renamed.name = "details".to_owned();

    let plan = DatabasePlan {
        steps: vec![DatabasePlanStep::AlterTable {
            schema: Some("shop".to_owned()),
            table: "events".to_owned(),
            change: Box::new(TablePlanStep::AlterColumn {
                type_cast: None,
                before: column("description"),
                after: renamed,
            }),
        }],
    };

    let error = Mysql.render_plan(&plan, &mut Vec::new()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn mysql_renders_identity_and_generated_transitions() {
    let mut id_before = column("id");
    id_before.ty = SqlType::I32;
    id_before.nullable = false;
    let mut id_after = id_before.clone();
    id_after.identity = Some(IdentityModel {
        mode: IdentityMode::AutoIncrement,
    });

    let total_before = column("total");
    let mut total_after = total_before.clone();
    total_after.generated = Some(GeneratedColumnModel {
        expression: "`a` + `b`".to_owned(),
        storage: GeneratedStorage::Virtual,
    });

    let alter = |before: ColumnModel, after: ColumnModel| DatabasePlanStep::AlterTable {
        schema: Some("shop".to_owned()),
        table: "events".to_owned(),
        change: Box::new(TablePlanStep::AlterColumn {
            before,
            after,
            type_cast: None,
        }),
    };
    let plan = DatabasePlan {
        steps: vec![alter(id_before, id_after), alter(total_before, total_after)],
    };

    let mut sql = Vec::new();
    Mysql.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "ALTER TABLE `shop`.`events` MODIFY COLUMN `id` INT NOT NULL AUTO_INCREMENT;\n\
ALTER TABLE `shop`.`events` MODIFY COLUMN `total` TEXT GENERATED ALWAYS AS (`a` + `b`) VIRTUAL;"
    );
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

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
#[primary_key(columns = [tenant_id, id])]
struct Seat<'scope, C: ColumnMode = ColumnExpr> {
    tenant_id: C::Type<'scope, i32>,
    id: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct SeatShop {
    seats: Seat<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct SeatShopDb {
    shop: SeatShop,
}

#[test]
fn mysql_renders_compound_primary_key() {
    let model = DatabaseModel::from_database::<SeatShopDb>();
    let mut sql = Vec::new();
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("CONSTRAINT `pk_seats` PRIMARY KEY (`tenant_id`, `id`)"),
        "expected compound PRIMARY KEY in: {sql}"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
#[unique(columns = [organization_id, slug])]
struct Repository<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    organization_id: C::Type<'scope, i32>,
    slug: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct RepositoryShop {
    repositorys: Repository<'static, ColumnName>,
}

#[test]
fn mysql_backend_writes_composite_unique_ddl() {
    // The query-side single-table `write_table` path must also emit table-level `#[unique(..)]`
    // constraints, otherwise duplicates are allowed even though `render_create` forbids them.
    let mut sql = Vec::new();
    let tables = <RepositoryShop as Schema>::tables().collect::<Vec<_>>();
    Mysql.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("UNIQUE (`organization_id`, `slug`)"),
        "expected composite UNIQUE constraint in write_table output: {sql}"
    );
}

// A partial unique (`where = ...`) lowers to a partial index, which MySQL does not support.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
#[unique(columns = [organization_id, slug], where = |row| row.deleted_at.is_null())]
struct SoftRepository<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    organization_id: C::Type<'scope, i32>,
    slug: C::Type<'scope, String>,
    deleted_at: C::Type<'scope, Option<i64>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct SoftRepositoryShop {
    soft_repositorys: SoftRepository<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct SoftRepositoryDb {
    shop: SoftRepositoryShop,
}

#[test]
fn mysql_rejects_partial_unique_index_in_write_table() {
    let mut sql = Vec::new();
    let tables = <SoftRepositoryShop as Schema>::tables().collect::<Vec<_>>();
    let error = Mysql.write_table(tables[0], &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_partial_unique_index_in_render_create() {
    let model = DatabaseModel::from_database::<SoftRepositoryDb>();
    let mut sql = Vec::new();
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

// The single-column `#[column(unique, where = ...)]` form, carried on `Column::unique_predicate()`.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
struct SoftAccount<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    #[column(unique, where = |row| row.deleted_at.is_null())]
    email: C::Type<'scope, String>,
    deleted_at: C::Type<'scope, Option<i64>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct SoftAccountShop {
    soft_accounts: SoftAccount<'static, ColumnName>,
}

#[test]
fn mysql_rejects_column_level_partial_unique_in_write_table() {
    // The column form is not in `table.uniques()`, so the direct path must still reject it rather
    // than silently emit a table without the intended uniqueness.
    let mut sql = Vec::new();
    let tables = <SoftAccountShop as Schema>::tables().collect::<Vec<_>>();
    let error = Mysql.write_table(tables[0], &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_renders_create_from_scratch() {
    let model = DatabaseModel::from_database::<ShopDb>();
    let mut sql = Vec::new();
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // MySQL dialect: backtick quoting, INT/VARCHAR(255)/unsigned types, AUTO_INCREMENT, FK as ALTER.
    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS `shop`;\n\
CREATE TABLE `shop`.`tenants` (\n  `id` INT NOT NULL AUTO_INCREMENT,\n  `slug` VARCHAR(255) NOT NULL,\n  CONSTRAINT `pk_tenants` PRIMARY KEY (`id`),\n  CONSTRAINT `uq_tenants_slug` UNIQUE (`slug`)\n);\n\
CREATE TABLE `shop`.`memberships` (\n  `id` INT NOT NULL AUTO_INCREMENT,\n  `tenant_id` INT NOT NULL,\n  `seats` SMALLINT UNSIGNED NOT NULL,\n  CONSTRAINT `pk_memberships` PRIMARY KEY (`id`)\n);\n\
CREATE INDEX `idx_memberships_tenant_id` ON `shop`.`memberships` (`tenant_id`);\n\
ALTER TABLE `shop`.`memberships` ADD CONSTRAINT `fk_memberships_tenant_id` FOREIGN KEY (`tenant_id`) REFERENCES `shop`.`tenants` (`id`) ON DELETE CASCADE;"
    );
}

#[test]
fn mysql_renders_table_and_column_comments() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
            views: Vec::new(),
            tables: vec![TableModel {
                name: "tenants".to_owned(),
                comment: Some("Tenant records".to_owned()),
                columns: vec![ColumnModel {
                    name: "slug".to_owned(),
                    comment: Some("Tenant's stable slug".to_owned()),
                    ty: SqlType::String,
                    collation: Some("utf8mb4_bin".to_owned()),
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
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS `shop`;\n\
CREATE TABLE `shop`.`tenants` (\n  `slug` VARCHAR(255) COLLATE utf8mb4_bin NOT NULL COMMENT 'Tenant''s stable slug'\n) COMMENT='Tenant records';"
    );
}

#[test]
fn mysql_rejects_foreign_key_match_types() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
                foreign_keys: vec![ForeignKeyModel {
                    name: "fk_memberships_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                    references_schema: Some("shop".to_owned()),
                    references_table: "tenants".to_owned(),
                    references_columns: vec!["id".to_owned()],
                    match_type: Some(ForeignKeyMatch::Full),
                    deferrability: None,
                    validation: None,
                    enforcement: None,
                    on_delete: None,
                    on_update: None,
                }],
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    };

    let mut sql = Vec::new();
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_deferrable_foreign_keys() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
                foreign_keys: vec![ForeignKeyModel {
                    name: "fk_memberships_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                    references_schema: Some("shop".to_owned()),
                    references_table: "tenants".to_owned(),
                    references_columns: vec!["id".to_owned()],
                    match_type: None,
                    deferrability: Some(ConstraintDeferrability::InitiallyDeferred),
                    validation: None,
                    enforcement: None,
                    on_delete: None,
                    on_update: None,
                }],
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
        }],
    };

    let mut sql = Vec::new();
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_check_constraint_enforcement() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
                checks: vec![CheckModel {
                    name: "ck_memberships_tenant_id".to_owned(),
                    expression: "tenant_id > 0".to_owned(),
                    validation: None,
                    enforcement: Some(ConstraintEnforcement::NotEnforced),
                }],
                indexes: Vec::new(),
            }],
        }],
    };

    let mut sql = Vec::new();
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_partial_index_predicates() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
                    directions: vec![IndexDirection::Asc],
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate: Some("tenant_id > 0".to_owned()),
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_expression_indexes() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_covering_index_include_columns() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_index_null_ordering() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_index_operator_classes() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn mysql_rejects_index_collations() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
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
                    operator_classes: Vec::new(),
                    predicate: None,
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

/// Builds a single-column `id` table optionally referencing `references_table` via a foreign key, for
/// the multi-table create-ordering test below.
fn fk_test_table(name: &str, references_table: Option<&str>) -> Box<TableModel> {
    let foreign_keys = references_table
        .map(|target| ForeignKeyModel {
            name: format!("fk_{name}_{target}"),
            columns: vec!["id".to_owned()],
            references_schema: Some("shop".to_owned()),
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
fn mysql_defers_foreign_keys_until_all_tables_are_created() {
    // `comments` is created first but references `posts`, created second. The foreign key must be
    // deferred until after both `CREATE TABLE`s, or it would reference a table that does not exist yet.
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::CreateTable {
                schema: Some("shop".to_owned()),
                table: fk_test_table("comments", Some("posts")),
            },
            DatabasePlanStep::CreateTable {
                schema: Some("shop".to_owned()),
                table: fk_test_table("posts", None),
            },
        ],
    };

    let mut sql = Vec::new();
    Mysql.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    let comments_create = sql.find("CREATE TABLE `shop`.`comments`").unwrap();
    let posts_create = sql.find("CREATE TABLE `shop`.`posts`").unwrap();
    let fk = sql.find("ADD CONSTRAINT `fk_comments_posts`").unwrap();
    assert!(
        comments_create < posts_create && posts_create < fk,
        "foreign key not deferred until after both tables were created: {sql}"
    );
}

// View rendering: the structural body becomes `CREATE VIEW … AS SELECT …`, emitted after tables.
// Structural identifiers use MySQL backticks; the canonical expression fragments use ANSI double
// quotes (so the statement must run under `ANSI_QUOTES`).
#[test]
fn mysql_renders_view_after_tables() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
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
            views: vec![ViewModel {
                name: "active_users".to_owned(),
                comment: None,
                columns: vec![ViewColumnModel {
                    name: "id".to_owned(),
                    ty: SqlType::I32,
                    nullable: false,
                }],
                query: ViewQueryModel {
                    dependencies: Vec::new(),
                    distinct: false,
                    projection: vec![ProjectionItem {
                        output_name: "id".to_owned(),
                        expr: ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "id".to_owned(),
                        },
                    }],
                    from: Some(SourceRef {
                        schema: Some("app".to_owned()),
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
            }],
        }],
    };

    let mut sql = Vec::new();
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE VIEW `app`.`active_users` (`id`) AS \
SELECT q0_0.`id` FROM `app`.`users` AS q0_0 WHERE (q0_0.`id` > 0)"
        ),
        "unexpected view DDL: {sql}"
    );
    assert!(
        sql.find("CREATE TABLE").unwrap() < sql.find("CREATE VIEW").unwrap(),
        "view must be created after tables: {sql}"
    );
}

// Re-quoting identifiers to backticks must not touch single-quoted string literals: a `"` that is part
// of a string value stays, while the `"`-quoted column identifier becomes a backtick.
#[test]
fn mysql_view_fragment_requoting_preserves_string_literals() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: Vec::new(),
            views: vec![ViewModel {
                name: "tricky".to_owned(),
                comment: None,
                columns: vec![ViewColumnModel {
                    name: "name".to_owned(),
                    ty: SqlType::String,
                    nullable: false,
                }],
                query: ViewQueryModel {
                    dependencies: Vec::new(),
                    distinct: false,
                    projection: vec![ProjectionItem {
                        output_name: "name".to_owned(),
                        expr: ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "name".to_owned(),
                        },
                    }],
                    from: Some(SourceRef {
                        schema: None,
                        name: "people".to_owned(),
                        alias: "q0_0".to_owned(),
                    }),
                    joins: Vec::new(),
                    // A string literal that itself contains a double quote.
                    filter: Some(ExprNode::Compare {
                        op: CompareOp::Equals,
                        left: Box::new(ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "name".to_owned(),
                        }),
                        right: Box::new(ExprNode::Literal("'a\"b'".to_owned())),
                    }),
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
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("SELECT q0_0.`name` FROM `people` AS q0_0 WHERE (q0_0.`name` = 'a\"b')"),
        "fragment requoting wrong: {sql}"
    );
}

// Incremental plan rendering: CreateView -> CREATE OR REPLACE VIEW, DropView -> DROP VIEW, with
// fragments re-quoted to backticks.
#[test]
fn mysql_renders_view_plan_steps() {
    let view = ViewModel {
        name: "active_users".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "id".to_owned(),
            ty: SqlType::I32,
            nullable: false,
        }],
        query: ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                expr: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "id".to_owned(),
                },
            }],
            from: Some(SourceRef {
                schema: Some("app".to_owned()),
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
                schema: Some("app".to_owned()),
                view: Box::new(view.clone()),
            },
            DatabasePlanStep::DropView {
                schema: Some("app".to_owned()),
                view: Box::new(view),
            },
        ],
    };

    let mut sql = Vec::new();
    Mysql.render_plan(&plan, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE OR REPLACE VIEW `app`.`active_users` (`id`) AS \
SELECT q0_0.`id` FROM `app`.`users` AS q0_0 WHERE (q0_0.`id` > 0)"
        ),
        "missing create-or-replace: {sql}"
    );
    assert!(
        sql.contains("DROP VIEW `app`.`active_users`"),
        "missing drop view: {sql}"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Vault)]
struct Secret<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    ciphertext: C::Type<'scope, Vec<u8>>,
    wrapped_dek: C::Type<'scope, Option<Vec<u8>>>,
    key: C::Type<'scope, [u8; 32]>,
    nonce: C::Type<'scope, Option<[u8; 12]>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Vault {
    secrets: Secret<'static, ColumnName>,
}

#[test]
fn mysql_writes_blob_column_ddl() {
    let mut sql = Vec::new();
    let tables = <Vault as Schema>::tables().collect::<Vec<_>>();
    Mysql.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // A `Vec<u8>` column renders as non-null `BLOB`; the `Option<Vec<u8>>` column as nullable `BLOB`.
    assert!(sql.contains("`ciphertext` BLOB NOT NULL"), "{sql}");
    assert!(sql.contains("`wrapped_dek` BLOB"), "{sql}");
    assert!(
        !sql.contains("`wrapped_dek` BLOB NOT NULL"),
        "nullable BLOB must not be NOT NULL: {sql}"
    );
}

#[test]
fn mysql_writes_fixed_bytes_column_ddl() {
    let mut sql = Vec::new();
    let tables = <Vault as Schema>::tables().collect::<Vec<_>>();
    Mysql.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // A `[u8; N]` column renders as `BINARY(N)` (the width is native, no CHECK needed).
    assert!(sql.contains("`key` BINARY(32) NOT NULL"), "{sql}");
    assert!(sql.contains("`nonce` BINARY(12)"), "{sql}");
    assert!(
        !sql.contains("`nonce` BINARY(12) NOT NULL"),
        "nullable fixed-bytes must not be NOT NULL: {sql}"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(BigVault)]
struct BigSecret<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    huge: C::Type<'scope, [u8; 256]>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct BigVault {
    big_secrets: BigSecret<'static, ColumnName>,
}

#[test]
fn mysql_rejects_fixed_bytes_wider_than_binary_limit() {
    // MySQL `BINARY(M)` caps at 255 bytes, so a wider `[u8; N]` must fail to render rather than emit
    // DDL the server rejects.
    let mut sql = Vec::new();
    let tables = <BigVault as Schema>::tables().collect::<Vec<_>>();
    let error = Mysql.write_table(tables[0], &mut sql).unwrap_err();
    assert!(error.to_string().contains("255"), "{error}");
}

// The same structural expression IR renders in MySQL's dialect: `/` is already fractional (no float
// cast), identifiers use backticks, and aggregate casts use MySQL's `SIGNED`.
#[test]
fn mysql_renders_view_expression_ir_in_its_dialect() {
    fn col(c: &str) -> ExprNode {
        ExprNode::Column {
            alias: "q0_0".to_owned(),
            column: c.to_owned(),
        }
    }
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
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
                    dependencies: Vec::new(),
                    distinct: false,
                    projection: vec![
                        ProjectionItem {
                            output_name: "ratio".to_owned(),
                            expr: ExprNode::Binary {
                                op: ArithmeticOp::Divide,
                                left: Box::new(col("count")),
                                right: Box::new(ExprNode::Literal("2".to_owned())),
                            },
                        },
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
                        schema: Some("app".to_owned()),
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
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // MySQL `/` is already fractional, so no float cast is injected — and identifiers are backticks.
    assert!(
        sql.contains("(q0_0.`count` / 2)"),
        "plain MySQL division missing: {sql}"
    );
    assert!(
        !sql.contains("double precision"),
        "MySQL must not get PG casts: {sql}"
    );
    // Aggregate result cast uses MySQL's `SIGNED`.
    assert!(
        sql.contains("CAST(SUM(q0_0.`amount`) AS SIGNED)"),
        "MySQL aggregate cast missing: {sql}"
    );
}

// MySQL has no `NULLS FIRST`/`NULLS LAST` syntax, so a view body carrying an explicit null ordering
// (e.g. from a package or hand-built model) must render without it rather than emitting invalid DDL.
#[test]
fn mysql_view_order_by_drops_nulls_modifier() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
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
                    dependencies: Vec::new(),
                    distinct: false,
                    projection: vec![ProjectionItem {
                        output_name: "id".to_owned(),
                        expr: ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "id".to_owned(),
                        },
                    }],
                    from: Some(SourceRef {
                        schema: Some("app".to_owned()),
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
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("ORDER BY q0_0.`id` DESC"),
        "expected ORDER BY direction: {sql}"
    );
    assert!(
        !sql.to_uppercase().contains("NULLS"),
        "MySQL must not emit a NULLS modifier: {sql}"
    );
}

// --- Upsert: `INSERT … ON DUPLICATE KEY UPDATE` ---

#[test]
fn mysql_upsert_do_update_renders_on_duplicate_key_update() {
    // Replace-all `do_update`: every inserted column is set to `VALUES(col)`. MySQL has no conflict
    // target, so the `on_conflict(|t| t.id)` target is omitted; `build()` renders without RETURNING.
    let sql = Mysql
        .to::<Tenant>()
        .slug("acme")
        .on_conflict(|tenant| tenant.id)
        .do_update()
        .build()
        .to_sql();
    assert_eq!(
        sql,
        "INSERT INTO `shop`.`tenants` (`slug`) VALUES (?) \
         ON DUPLICATE KEY UPDATE `slug` = VALUES(`slug`)"
    );
}

#[test]
fn mysql_upsert_multi_column_do_update_renders() {
    let sql = Mysql
        .to::<Membership>()
        .tenant_id(1)
        .seats(5u16)
        .on_conflict(|membership| membership.id)
        .do_update()
        .build()
        .to_sql();
    assert_eq!(
        sql,
        "INSERT INTO `shop`.`memberships` (`tenant_id`, `seats`) VALUES (?, ?) \
         ON DUPLICATE KEY UPDATE `tenant_id` = VALUES(`tenant_id`), `seats` = VALUES(`seats`)"
    );
}

#[test]
fn mysql_upsert_do_nothing_emulated_by_self_assigning_first_column() {
    // MySQL has no `DO NOTHING`; it self-assigns the first inserted column as a no-op update.
    let sql = Mysql
        .to::<Tenant>()
        .slug("acme")
        .on_conflict(|tenant| tenant.id)
        .do_nothing()
        .build()
        .to_sql();
    assert_eq!(
        sql,
        "INSERT INTO `shop`.`tenants` (`slug`) VALUES (?) \
         ON DUPLICATE KEY UPDATE `slug` = `slug`"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

#[test]
fn mysql_upsert_do_nothing_with_default_values_self_assigns_target() {
    // A column-less (`DEFAULT VALUES`) insert has no inserted column to self-assign, so the no-op
    // falls back to the conflict-target column — the clause is never silently dropped.
    let sql = Mysql
        .to::<Counter>()
        .on_conflict(|counter| counter.id)
        .do_nothing()
        .build()
        .to_sql();
    assert_eq!(
        sql,
        "INSERT INTO `shop`.`counters` () VALUES () ON DUPLICATE KEY UPDATE `id` = `id`"
    );
}

#[test]
fn mysql_order_by_nulls_last_emulated_with_is_null_key() {
    // MySQL has no NULLS syntax; a leading `(<expr> IS NULL)` key emulates it. NULLS LAST => the key
    // sorts ASC (non-nulls before nulls).
    let sql = Mysql
        .from::<Tenant>()
        .order_by(|(tenant,)| tenant.slug.desc().nulls_last())
        .select(|(tenant,)| tenant.id)
        .to_sql();
    assert_eq!(
        sql,
        "SELECT q0_0.`id` AS `id` FROM `shop`.`tenants` AS q0_0 \
         ORDER BY (q0_0.`slug` IS NULL) ASC, q0_0.`slug` DESC"
    );
}

#[test]
fn mysql_order_by_nulls_first_emulated_with_is_null_key() {
    let sql = Mysql
        .from::<Tenant>()
        .order_by(|(tenant,)| tenant.slug.asc().nulls_first())
        .select(|(tenant,)| tenant.id)
        .to_sql();
    assert_eq!(
        sql,
        "SELECT q0_0.`id` AS `id` FROM `shop`.`tenants` AS q0_0 \
         ORDER BY (q0_0.`slug` IS NULL) DESC, q0_0.`slug` ASC"
    );
}

#[test]
fn mysql_for_update_renders() {
    let sql = Mysql
        .from::<Tenant>()
        .for_update()
        .select(|(tenant,)| tenant.id)
        .to_sql();
    assert_eq!(
        sql,
        "SELECT q0_0.`id` AS `id` FROM `shop`.`tenants` AS q0_0 FOR UPDATE"
    );
}

#[test]
fn mysql_for_share_renders_lock_in_share_mode() {
    let sql = Mysql
        .from::<Tenant>()
        .for_share()
        .select(|(tenant,)| tenant.id)
        .to_sql();
    assert_eq!(
        sql,
        "SELECT q0_0.`id` AS `id` FROM `shop`.`tenants` AS q0_0 LOCK IN SHARE MODE"
    );
}

#[test]
fn mysql_insert_select_renders() {
    // INSERT INTO t (cols) SELECT … with `?` placeholders.
    let conn = Mysql;
    let q = conn.to::<Tenant>().insert_select(
        |tenant| tenant.slug,
        conn.from::<Tenant>()
            .where_(|tenant| tenant.id.greater_than(10))
            .select(|(tenant,)| tenant.slug),
    );
    assert_eq!(
        q.to_sql(),
        "INSERT INTO `shop`.`tenants` (`slug`) \
         SELECT q0_0.`slug` AS `slug` FROM `shop`.`tenants` AS q0_0 WHERE (q0_0.`id` > ?)"
    );
}

#[test]
fn mysql_insert_select_multi_column_renders() {
    // Multi-column target list (tuple of columns) — exercises the wider-arity column-list path.
    let conn = Mysql;
    let q = conn.to::<Membership>().insert_select(
        |membership| (membership.tenant_id, membership.seats),
        conn.from::<Membership>()
            .select(|(membership,)| (membership.tenant_id, membership.seats)),
    );
    assert_eq!(
        q.to_sql(),
        "INSERT INTO `shop`.`memberships` (`tenant_id`, `seats`) \
         SELECT q0_0.`tenant_id` AS `t0_tenant_id`, q0_0.`seats` AS `t1_seats` \
         FROM `shop`.`memberships` AS q0_0"
    );
}

#[test]
fn mysql_update_from_renders_join() {
    // MySQL renders a correlated update as `JOIN other AS b ON <correlation> SET …` (no `FROM`).
    let update = Mysql
        .to_columns::<Membership, (MembershipTenantId,)>()
        .from::<Tenant>()
        .set(|(_membership, tenant)| (tenant.id,))
        .where_(|(membership, tenant)| membership.tenant_id.equals(tenant.id))
        .build();
    assert_eq!(
        update.to_sql(),
        "UPDATE `shop`.`memberships` AS q0_0 \
         JOIN `shop`.`tenants` AS q0_1 ON (q0_0.`tenant_id` = q0_1.`id`) \
         SET q0_0.`tenant_id` = q0_1.`id`"
    );
}

#[test]
fn mysql_delete_using_renders_join() {
    // MySQL renders a correlated delete as `DELETE a FROM t AS a JOIN other AS b ON <corr>` — the
    // leading alias selects which table's rows are deleted.
    let delete = Mysql
        .from::<Membership>()
        .using::<Tenant>()
        .where_(|(membership, tenant)| membership.tenant_id.equals(tenant.id))
        .build();
    assert_eq!(
        delete.to_sql(),
        "DELETE q0_0 FROM `shop`.`memberships` AS q0_0 \
         JOIN `shop`.`tenants` AS q0_1 ON (q0_0.`tenant_id` = q0_1.`id`)"
    );
}
