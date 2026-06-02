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

    assert!(!capabilities.constraints.foreign_key_validation);
    assert!(!capabilities.constraints.foreign_key_enforcement);
    assert!(!capabilities.constraints.check_validation);
    assert!(!capabilities.constraints.check_enforcement);
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
