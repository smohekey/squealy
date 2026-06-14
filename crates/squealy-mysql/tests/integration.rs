//! Live MySQL round-trip: render + execute insert and select through the runtime.
//!
//! `#[ignore]`d like the other backend integration tests; run with a database via:
//! `SQUEALY_MYSQL_URL=... cargo test -p squealy-mysql --test integration -- --ignored`.

use squealy::*;
use squealy_mysql::Mysql;

#[derive(Clone, Debug, PartialEq, Table)]
struct RuntimeWidget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn database_url() -> String {
    std::env::var("SQUEALY_MYSQL_URL")
        .unwrap_or_else(|_| "mysql://root:root@127.0.0.1:33306/squealy_test".to_owned())
}

#[tokio::test]
#[ignore]
async fn mysql_round_trips_insert_and_select() {
    let mut connection = Mysql
        .connect(&database_url())
        .await
        .expect("connect to MySQL");

    connection
        .execute_ddl("DROP TABLE IF EXISTS runtime_widgets")
        .await
        .expect("drop table");
    connection
        .execute_ddl(
            "CREATE TABLE runtime_widgets (\
`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
`name` VARCHAR(255) NOT NULL)",
        )
        .await
        .expect("create table");

    // Insert two rows through the runtime (rendered with `?` placeholders, executed via MysqlRows).
    let affected = connection
        .to::<RuntimeWidget>()
        .name("gadget")
        .insert()
        .await
        .expect("insert gadget");
    assert_eq!(affected, 1);
    connection
        .to::<RuntimeWidget>()
        .name("widget")
        .insert()
        .await
        .expect("insert widget");

    // Select them back, filtered and ordered, decoding rows through the value codec.
    let names = connection
        .from::<RuntimeWidget>()
        .where_(|widget| widget.name.equals("gadget"))
        .select(|(widget,)| widget.name)
        .collect()
        .await
        .expect("select gadget");
    assert_eq!(names, vec!["gadget".to_owned()]);

    let all = connection
        .from::<RuntimeWidget>()
        .order_by(|(widget,)| widget.id.asc())
        .select(|(widget,)| (widget.id, widget.name))
        .collect()
        .await
        .expect("select all");
    assert_eq!(
        all,
        vec![(1, "gadget".to_owned()), (2, "widget".to_owned())]
    );

    connection
        .execute_ddl("DROP TABLE IF EXISTS runtime_widgets")
        .await
        .expect("cleanup");
}
