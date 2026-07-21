use std::io::Write;
use std::process::Command;

use squealy::*;
use squealy_ddl_sqlite::render_create_sql;
use squealy_sqlite::SqliteConnection;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct Owner<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key)]
	id: C::Type<'scope, [u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct Project<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key)]
	id: C::Type<'scope, [u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct Repository<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key)]
	id: C::Type<'scope, [u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct Worktree<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key)]
	id: C::Type<'scope, [u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct Session<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key)]
	id: C::Type<'scope, [u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct Job<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key)]
	id: C::Type<'scope, [u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
#[unique(name = "uq_records_stage_outcome", columns = [stage, outcome])]
#[index(name = "idx_records_id", columns = [id])]
#[index(name = "idx_records_owner", columns = [owner_id])]
#[index(name = "idx_records_project", columns = [project_id])]
#[index(name = "idx_records_repository", columns = [repository_id])]
#[index(name = "idx_records_worktree", columns = [worktree_id])]
#[index(name = "idx_records_session", columns = [session_id])]
#[index(name = "idx_records_job", columns = [job_id])]
#[index(name = "idx_records_outcome_stage", columns = [outcome, stage])]
struct RichRecord<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key)]
	id: C::Type<'scope, [u8; 16]>,
	#[column(
		check = "length(owner_id) = 16",
		references(Owner::id, on_delete = "cascade")
	)]
	owner_id: C::Type<'scope, [u8; 16]>,
	#[column(
		check = "length(project_id) = 16",
		references(Project::id, on_delete = "cascade")
	)]
	project_id: C::Type<'scope, [u8; 16]>,
	#[column(
		check = "length(repository_id) = 16",
		references(Repository::id, on_delete = "cascade")
	)]
	repository_id: C::Type<'scope, [u8; 16]>,
	#[column(
		check = "length(worktree_id) = 16",
		references(Worktree::id, on_delete = "cascade")
	)]
	worktree_id: C::Type<'scope, [u8; 16]>,
	#[column(
		check = "length(session_id) = 16",
		references(Session::id, on_delete = "cascade")
	)]
	session_id: C::Type<'scope, [u8; 16]>,
	#[column(
		check = "length(job_id) = 16",
		references(Job::id, on_delete = "cascade")
	)]
	job_id: C::Type<'scope, [u8; 16]>,
	#[column(unique, check = "name <> ''")]
	name: C::Type<'scope, String>,
	#[column(unique, check = "slug <> ''")]
	slug: C::Type<'scope, String>,
	#[column(unique, check = "kind <> ''")]
	kind: C::Type<'scope, String>,
	#[column(check = "stage <> ''")]
	stage: C::Type<'scope, String>,
	#[column(check = "outcome <> ''")]
	outcome: C::Type<'scope, String>,
	#[column(default = value("queued"), check = "status <> ''")]
	status: C::Type<'scope, String>,
	#[column(unique, check = "source <> ''")]
	source: C::Type<'scope, String>,
	#[column(unique, check = "target <> ''")]
	target: C::Type<'scope, String>,
	#[column(unique, check = "label IS NULL OR label <> ''")]
	label: C::Type<'scope, Option<String>>,
	#[column(unique, check = "detail IS NULL OR detail <> ''")]
	detail: C::Type<'scope, Option<String>>,
	#[column(default = value(0), check = "attempts >= 0")]
	attempts: C::Type<'scope, i64>,
	#[column(check = "priority >= 0")]
	priority: C::Type<'scope, i64>,
	#[column(check = "created_at >= 0")]
	created_at: C::Type<'scope, i64>,
	#[column(check = "updated_at >= 0")]
	updated_at: C::Type<'scope, i64>,
	#[column(default = value(true), check = "active IN (0, 1)")]
	active: C::Type<'scope, bool>,
	#[column(check = "archived IN (0, 1)")]
	archived: C::Type<'scope, bool>,
	payload: C::Type<'scope, Vec<u8>>,
	#[column(unique, check = "length(digest) = 32")]
	digest: C::Type<'scope, [u8; 32]>,
	#[column(check = "optional_digest IS NULL OR length(optional_digest) = 32")]
	optional_digest: C::Type<'scope, Option<[u8; 32]>>,
	#[column(check = "optional_payload IS NULL OR length(optional_payload) > 0")]
	optional_payload: C::Type<'scope, Option<Vec<u8>>>,
	#[column(check = "optional_count IS NULL OR optional_count >= 0")]
	optional_count: C::Type<'scope, Option<i64>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct App {
	owners: Owner<'static, ColumnName>,
	projects: Project<'static, ColumnName>,
	repositories: Repository<'static, ColumnName>,
	worktrees: Worktree<'static, ColumnName>,
	sessions: Session<'static, ColumnName>,
	jobs: Job<'static, ColumnName>,
	rich_records: RichRecord<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct FixtureDatabase {
	app: App,
}

fn fixture_sql() -> String {
	let model = DatabaseModel::from_database::<FixtureDatabase>();
	let rich = &model.schemas[0].tables[6];
	assert_eq!(rich.columns.len(), 28);
	assert_eq!(rich.checks.len(), 26);
	assert_eq!(rich.uniques.len(), 9);
	assert_eq!(rich.indexes.len(), 8);
	assert_eq!(rich.foreign_keys.len(), 6);
	assert_eq!(
		rich
			.columns
			.iter()
			.filter(|column| column.default.is_some())
			.count(),
		3
	);
	render_create_sql(&model).expect("render fixture DDL")
}

#[test]
fn process_render_helper() {
	if std::env::var_os("SQUEALY_DDL_RENDER_CHILD").is_none() {
		return;
	}
	std::io::stdout()
		.write_all(fixture_sql().as_bytes())
		.expect("write rendered SQL");
	std::process::exit(0);
}

#[test]
fn rendering_is_identical_across_processes_and_matches_the_golden_file() {
	fn render_in_child() -> Vec<u8> {
		let output = Command::new(std::env::current_exe().expect("current test executable"))
			.arg("--exact")
			.arg("process_render_helper")
			.arg("--nocapture")
			.env("SQUEALY_DDL_RENDER_CHILD", "1")
			.output()
			.expect("spawn renderer test executable");
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		let start = output
			.stdout
			.windows(b"CREATE TABLE".len())
			.position(|window| window == b"CREATE TABLE")
			.expect("child output contains rendered SQL");
		output.stdout[start..].to_vec()
	}

	let first = render_in_child();
	let second = render_in_child();
	assert_eq!(first, second);
	let golden = include_bytes!("fixtures/rich_database.sql");
	assert_eq!(first, golden.strip_suffix(b"\n").unwrap_or(golden));
}

#[tokio::test]
async fn rich_fixture_executes_and_exposes_the_expected_sqlite_schema() {
	let driver = tokio_rusqlite::Connection::open_in_memory()
		.await
		.expect("open SQLite");
	let inspection = driver.clone();
	let connection = SqliteConnection::new(driver);
	connection
		.execute_batch("PRAGMA foreign_keys = ON")
		.await
		.expect("enable foreign keys");
	let sql = fixture_sql();
	connection.execute_batch(&sql).await.expect("execute DDL");

	assert_eq!(
		connection.list_user_tables().await.expect("list tables"),
		vec![
			"jobs".to_owned(),
			"owners".to_owned(),
			"projects".to_owned(),
			"repositorys".to_owned(),
			"rich_records".to_owned(),
			"sessions".to_owned(),
			"worktrees".to_owned(),
		]
	);

	let (columns, indexes, composite_columns, foreign_keys, stored_sql) = inspection
		.call(|conn| {
			let columns = conn
				.prepare("PRAGMA table_info(rich_records)")?
				.query_map([], |row| {
					Ok((
						row.get::<_, String>(1)?,
						row.get::<_, String>(2)?,
						row.get::<_, i64>(3)?,
						row.get::<_, Option<String>>(4)?,
					))
				})?
				.collect::<Result<Vec<_>, _>>()?;

			let indexes = conn
				.prepare("PRAGMA index_list(rich_records)")?
				.query_map([], |row| row.get::<_, String>(1))?
				.collect::<Result<Vec<_>, _>>()?;

			let composite_columns = conn
				.prepare("PRAGMA index_info(idx_records_outcome_stage)")?
				.query_map([], |row| row.get::<_, String>(2))?
				.collect::<Result<Vec<_>, _>>()?;

			let foreign_keys = conn
				.prepare("PRAGMA foreign_key_list(rich_records)")?
				.query_map([], |row| {
					Ok((row.get::<_, String>(2)?, row.get::<_, String>(6)?))
				})?
				.collect::<Result<Vec<_>, _>>()?;

			let stored_sql = conn.query_row(
				"SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'rich_records'",
				[],
				|row| row.get::<_, String>(0),
			)?;
			Ok::<_, tokio_rusqlite::rusqlite::Error>((
				columns,
				indexes,
				composite_columns,
				foreign_keys,
				stored_sql,
			))
		})
		.await
		.expect("inspect schema");

	assert_eq!(columns.len(), 28);
	assert_eq!(columns[0], ("id".to_owned(), "BLOB".to_owned(), 1, None));
	assert_eq!(columns[7].1, "TEXT");
	assert_eq!(columns[17].1, "INTEGER");
	assert_eq!(columns[23].1, "BLOB");
	assert_eq!(columns[25].2, 0);
	assert_eq!(columns[12].3.as_deref(), Some("'queued'"));
	assert_eq!(columns[17].3.as_deref(), Some("0"));
	assert_eq!(columns[21].3.as_deref(), Some("1"));

	let expected_indexes = [
		"idx_records_id",
		"idx_records_job",
		"idx_records_outcome_stage",
		"idx_records_owner",
		"idx_records_project",
		"idx_records_repository",
		"idx_records_session",
		"idx_records_worktree",
	];
	for expected in expected_indexes {
		assert!(indexes.iter().any(|name| name == expected));
	}
	assert_eq!(composite_columns, ["outcome", "stage"]);
	assert_eq!(foreign_keys.len(), 6);
	assert!(foreign_keys
		.iter()
		.all(|(_, on_delete)| on_delete == "CASCADE"));
	assert!(foreign_keys.iter().any(|(table, _)| table == "owners"));
	assert!(stored_sql.contains("length(CAST(\"id\" AS BLOB)) = 16"));
	assert!(stored_sql.contains("length(CAST(\"digest\" AS BLOB)) = 32"));
	assert!(!stored_sql.contains("CONSTRAINT"));
}

#[tokio::test]
async fn sqlite_rejects_check_unique_foreign_key_and_fixed_width_violations() {
	let connection = SqliteConnection::new(
		tokio_rusqlite::Connection::open_in_memory()
			.await
			.expect("open SQLite"),
	);
	connection
		.execute_batch("PRAGMA foreign_keys = ON")
		.await
		.expect("enable foreign keys");
	connection
		.execute_batch(&fixture_sql())
		.await
		.expect("create schema");
	connection
		.execute_batch(
			"INSERT INTO owners VALUES (X'00000000000000000000000000000001');\n\
			 INSERT INTO projects VALUES (X'00000000000000000000000000000001');\n\
			 INSERT INTO repositorys VALUES (X'00000000000000000000000000000001');\n\
			 INSERT INTO worktrees VALUES (X'00000000000000000000000000000001');\n\
			 INSERT INTO sessions VALUES (X'00000000000000000000000000000001');\n\
			 INSERT INTO jobs VALUES (X'00000000000000000000000000000001');",
		)
		.await
		.expect("create referenced rows");

	let insert = |id: &str, owner: &str, name: &str, slug: &str, digest: &str| {
		format!(
			"INSERT INTO rich_records (id, owner_id, project_id, repository_id, worktree_id, \
			 session_id, job_id, name, slug, kind, stage, outcome, source, target, priority, \
			 created_at, updated_at, archived, payload, digest) VALUES \
			 (X'{id}', X'{owner}', X'00000000000000000000000000000001', \
			 X'00000000000000000000000000000001', X'00000000000000000000000000000001', \
			 X'00000000000000000000000000000001', X'00000000000000000000000000000001', \
			 '{name}', '{slug}', '{name}-kind', '{name}-stage', '{name}-outcome', '{name}-source', \
			 '{name}-target', 1, 1, 1, 0, X'01', X'{digest}')"
		)
	};
	let valid_digest = "0000000000000000000000000000000000000000000000000000000000000001";
	connection
		.execute_batch(&insert(
			"00000000000000000000000000000010",
			"00000000000000000000000000000001",
			"valid",
			"valid",
			valid_digest,
		))
		.await
		.expect("insert valid row");

	let cases = [
		(
			insert(
				"00000000000000000000000000000011",
				"00000000000000000000000000000001",
				"",
				"check",
				"1000000000000000000000000000000000000000000000000000000000000001",
			),
			"CHECK constraint failed",
		),
		(
			insert(
				"00000000000000000000000000000012",
				"00000000000000000000000000000001",
				"valid",
				"valid",
				valid_digest,
			),
			"UNIQUE constraint failed",
		),
		(
			insert(
				"00000000000000000000000000000013",
				"00000000000000000000000000000002",
				"foreign",
				"foreign",
				"2000000000000000000000000000000000000000000000000000000000000001",
			),
			"FOREIGN KEY constraint failed",
		),
		(
			insert(
				"14",
				"00000000000000000000000000000001",
				"width",
				"width",
				"3000000000000000000000000000000000000000000000000000000000000001",
			),
			"CHECK constraint failed",
		),
	];
	for (sql, expected) in cases {
		let error = connection
			.execute_batch(&sql)
			.await
			.expect_err("constraint must reject row");
		assert!(error.to_string().contains(expected), "{error}");
	}
}
