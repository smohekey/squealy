//! Live MySQL round-trip: render + execute insert and select through the runtime.
//!
//! `#[ignore]`d like the other backend integration tests; run with a database via:
//! `SQUEALY_MYSQL_URL=... cargo test -p squealy-mysql --test integration -- --ignored`.

use mysql_async::prelude::Queryable;
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

async fn execute_fixture(sql: &str) {
	let mut conn = mysql_async::Conn::from_url(&database_url())
		.await
		.expect("connect fixture client");
	conn.query_drop(sql).await.expect("execute fixture SQL");
}

#[tokio::test]
#[ignore]
async fn mysql_round_trips_insert_and_select() {
	execute_fixture("DROP TABLE IF EXISTS runtime_widgets").await;
	execute_fixture(
		"CREATE TABLE runtime_widgets (\
`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
`name` VARCHAR(255) NOT NULL)",
	)
	.await;

	let connection = Mysql
		.connect(&database_url())
		.await
		.expect("connect to MySQL");

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

	drop(connection);
	execute_fixture("DROP TABLE IF EXISTS runtime_widgets").await;
}

#[derive(Clone, Debug, PartialEq, Table)]
struct RuntimeAccount<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	#[column(unique)]
	slug: C::Type<'scope, String>,
	label: C::Type<'scope, String>,
}

#[tokio::test]
#[ignore]
async fn mysql_round_trips_upsert_on_duplicate_key() {
	execute_fixture("DROP TABLE IF EXISTS runtime_accounts").await;
	execute_fixture(
		"CREATE TABLE runtime_accounts (\
`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
`slug` VARCHAR(255) NOT NULL UNIQUE, \
`label` VARCHAR(255) NOT NULL)",
	)
	.await;

	let connection = Mysql
		.connect(&database_url())
		.await
		.expect("connect to MySQL");

	async fn label(connection: &squealy_mysql::MysqlConnection) -> Vec<String> {
		connection
			.from::<RuntimeAccount>()
			.where_(|account| account.slug.equals("acme"))
			.select(|(account,)| account.label)
			.collect()
			.await
			.expect("select label")
	}

	// First insert.
	connection
		.to::<RuntimeAccount>()
		.slug("acme")
		.label("first")
		.insert()
		.await
		.expect("initial insert");
	assert_eq!(label(&connection).await, vec!["first".to_owned()]);

	// `do_update` on the duplicate `slug` key replaces every inserted column with the proposed values.
	connection
		.to::<RuntimeAccount>()
		.slug("acme")
		.label("second")
		.on_conflict(|account| account.id)
		.do_update()
		.insert()
		.await
		.expect("upsert do_update");
	assert_eq!(label(&connection).await, vec!["second".to_owned()]);

	// `do_nothing` on the duplicate key leaves the existing row unchanged.
	connection
		.to::<RuntimeAccount>()
		.slug("acme")
		.label("third")
		.on_conflict(|account| account.id)
		.do_nothing()
		.insert()
		.await
		.expect("upsert do_nothing");
	assert_eq!(label(&connection).await, vec!["second".to_owned()]);

	drop(connection);
	execute_fixture("DROP TABLE IF EXISTS runtime_accounts").await;
}
