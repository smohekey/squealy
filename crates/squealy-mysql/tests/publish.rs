//! Live end-to-end test: render create-from-scratch and execute it against MySQL.
//!
//! `#[ignore]`d like the other backend integration tests; run with a database via:
//! `SQUEALY_MYSQL_URL=... cargo test -p squealy-mysql --test publish -- --ignored`.

use squealy::*;
use squealy_mysql::Mysql;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    name: C::Type<'scope, String>,
    seats: C::Type<'scope, u32>,
}

// A referencing table so the live test exercises FK creation (the `ALTER … ADD CONSTRAINT` MySQL is
// strict about). `widget_id` matches `Widget::id` in size and sign.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Part<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(index, references(Widget::id, on_delete = "cascade"))]
    widget_id: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Catalog {
    widgets: Widget<'static, ColumnName>,
    parts: Part<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct CatalogDb {
    catalog: Catalog,
}

fn database_url() -> String {
    std::env::var("SQUEALY_MYSQL_URL")
        .unwrap_or_else(|_| "mysql://root:root@127.0.0.1:33306/squealy_test".to_owned())
}

#[tokio::test]
#[ignore]
async fn publishes_create_from_scratch() {
    let model = DatabaseModel::from_database::<CatalogDb>();
    let mut sql = Vec::new();
    Mysql.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");

    // Clean slate — the `catalog` schema is a MySQL database. (Re-runnable: render emits
    // CREATE TABLE, not IF NOT EXISTS.)
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog`")
        .await
        .expect("drop schema");

    // The whole script applies.
    connection
        .execute_ddl(&sql)
        .await
        .expect("create-from-scratch");

    // Re-running must fail because the objects now exist — proof they were created.
    assert!(
        connection.execute_ddl(&sql).await.is_err(),
        "re-running create-from-scratch should fail: objects already exist"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `catalog`")
        .await
        .expect("cleanup");
}
