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
                table: TableModel {
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
                },
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::AddColumn {
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
                },
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::DropIndex {
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
                },
            },
            DatabasePlanStep::DropTable {
                schema: Some("shop".to_owned()),
                table: TableModel {
                    name: "old_events".to_owned(),
                    comment: None,
                    columns: Vec::new(),
                    primary_key: None,
                    foreign_keys: Vec::new(),
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: Vec::new(),
                },
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
                change: TablePlanStep::AlterPrimaryKey {
                    before: Constraint {
                        name: "pk_events".to_owned(),
                        columns: vec!["id".to_owned()],
                    },
                    after: Constraint {
                        name: "pk_events".to_owned(),
                        columns: vec!["event_id".to_owned()],
                    },
                },
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::AlterUnique {
                    before: Constraint {
                        name: "uq_events_name".to_owned(),
                        columns: vec!["name".to_owned()],
                    },
                    after: Constraint {
                        name: "uq_events_name".to_owned(),
                        columns: vec!["slug".to_owned()],
                    },
                },
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::AlterForeignKey {
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
                },
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::AlterCheck {
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
                },
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::AlterIndex {
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
                },
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
                change: TablePlanStep::AlterColumn {
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
                },
            },
            DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::AlterColumn {
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
                },
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
                change: TablePlanStep::RenameColumn {
                    refactor_id: None,
                    from: "display_name".to_owned(),
                    to: "name".to_owned(),
                },
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
                change: TablePlanStep::RenameColumn {
                    refactor_id: Some("rename-display-name".to_owned()),
                    from: "display_name".to_owned(),
                    to: "name".to_owned(),
                },
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
    let mut renamed = column("description");
    renamed.name = "details".to_owned();

    let mut identity = column("description");
    identity.identity = Some(IdentityModel {
        mode: IdentityMode::AutoIncrement,
    });

    let mut generated = column("description");
    generated.generated = Some(GeneratedColumnModel {
        expression: "char_length(`description`)".to_owned(),
        storage: GeneratedStorage::Virtual,
    });

    for after in [renamed, identity, generated] {
        let plan = DatabasePlan {
            steps: vec![DatabasePlanStep::AlterTable {
                schema: Some("shop".to_owned()),
                table: "events".to_owned(),
                change: TablePlanStep::AlterColumn {
                    before: column("description"),
                    after,
                },
            }],
        };

        let mut sql = Vec::new();
        let error = Mysql.render_plan(&plan, &mut sql).unwrap_err();
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
