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
fn mysql_rejects_partial_index_predicates() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("shop".to_owned()),
            tables: vec![TableModel {
                name: "memberships".to_owned(),
                columns: vec![ColumnModel {
                    name: "tenant_id".to_owned(),
                    ty: SqlType::I32,
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
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Asc],
                    predicate: Some("tenant_id > 0".to_owned()),
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    let error = Mysql.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
