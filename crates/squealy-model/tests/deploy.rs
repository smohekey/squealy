use squealy::*;
use squealy_model::{DatabaseModel, render_create_sql, script};
use squealy_postgresql::Postgres;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Shop)]
struct Product<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    sku: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Shop {
    products: Product<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct ShopDb {
    shop: Shop,
}

#[test]
fn script_renders_create_from_scratch() {
    let sql = script::<ShopDb, _>(&Postgres).unwrap();

    assert!(
        sql.contains("CREATE SCHEMA IF NOT EXISTS \"shop\";"),
        "missing schema statement: {sql}"
    );
    assert!(
        sql.contains("CREATE TABLE \"shop\".\"products\" ("),
        "missing table statement: {sql}"
    );
    assert!(
        sql.contains("CONSTRAINT \"uq_products_sku\" UNIQUE (\"sku\")"),
        "missing unique constraint: {sql}"
    );
}

#[test]
fn script_matches_render_from_walked_model() {
    let from_db = script::<ShopDb, _>(&Postgres).unwrap();
    let model = DatabaseModel::from_database::<ShopDb>();
    let from_model = render_create_sql(&model, &Postgres).unwrap();
    assert_eq!(from_db, from_model);
}
