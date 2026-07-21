//! SQLite query backend for `squealy`.

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;

use squealy::ConnectionWithTransaction;

mod query;
mod sql;

pub use query::{SqliteRowReader, SqliteTransaction, SqliteValue};

/// The SQLite query backend marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sqlite;

impl Sqlite {
	/// This backend's SQL query dialect.
	pub fn dialect(&self) -> impl squealy::Dialect {
		crate::sql::SqliteDialect
	}
}

impl squealy::SupportsNamedWindow for Sqlite {}

#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
	#[error("sqlite connect error: {0}")]
	Connect(#[source] tokio_rusqlite::rusqlite::Error),
	#[error("sqlite setup error: {0}")]
	Setup(#[source] tokio_rusqlite::Error),
	#[error("sqlite query error: {0}")]
	Query(#[source] tokio_rusqlite::Error),
	#[error("query returned no rows")]
	NoRows,
	#[error("row is missing column {0}")]
	MissingColumn(usize),
	#[error("could not decode a {target} from a SQLite {found} value")]
	Decode {
		target: &'static str,
		found: &'static str,
	},
	#[error("could not convert value to {0}")]
	Conversion(&'static str),
	#[error("sqlite render error: {0}")]
	Render(#[source] std::io::Error),
}

/// A live SQLite connection backed by `tokio-rusqlite`'s worker thread.
pub struct SqliteConnection {
	pub(crate) conn: tokio_rusqlite::Connection,
}

impl SqliteConnection {
	/// Wraps an existing driver connection without changing its pragmas.
	///
	/// [`Sqlite::connect`](squealy::Connect::connect) enables foreign-key enforcement.
	pub fn new(conn: tokio_rusqlite::Connection) -> Self {
		Self { conn }
	}

	/// Executes one or more semicolon-separated SQL statements.
	///
	/// This delegates directly to [`rusqlite::Connection::execute_batch`][execute-batch]. It does not
	/// add an implicit transaction; callers that need atomicity must include transaction control in
	/// `sql` or use [`ConnectionWithTransaction`].
	///
	/// [execute-batch]: tokio_rusqlite::rusqlite::Connection::execute_batch
	pub async fn execute_batch(&self, sql: &str) -> Result<(), SqliteError> {
		let sql = sql.to_owned();
		self
			.conn
			.call(move |conn| conn.execute_batch(&sql))
			.await
			.map_err(SqliteError::Query)
	}

	/// Lists application table names in SQLite's database-wide namespace.
	///
	/// SQLite's internal `sqlite_*` tables and legacy Squealy bookkeeping tables named
	/// `__squealy_*` are excluded. The returned names are sorted by their UTF-8 bytes so the result is
	/// stable regardless of insertion order or database collation settings.
	pub async fn list_user_tables(&self) -> Result<Vec<String>, SqliteError> {
		let mut tables = self
			.conn
			.call(|conn| {
				let mut statement = conn.prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?;
				let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
				rows.collect::<Result<Vec<_>, _>>()
			})
			.await
			.map_err(SqliteError::Query)?;

		tables.retain(|name| {
			!name.as_bytes().starts_with(b"sqlite_") && !name.as_bytes().starts_with(b"__squealy_")
		});
		tables.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
		Ok(tables)
	}
}

impl std::fmt::Debug for SqliteConnection {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.debug_struct("SqliteConnection").finish()
	}
}

impl squealy::Connect for Sqlite {
	type Connection = SqliteConnection;
	type Error = SqliteError;

	async fn connect(&self, url: &str) -> Result<SqliteConnection, SqliteError> {
		let conn = if url.starts_with("file:") {
			tokio_rusqlite::Connection::open_with_flags(
				url,
				tokio_rusqlite::OpenFlags::default() | tokio_rusqlite::OpenFlags::SQLITE_OPEN_URI,
			)
			.await
		} else {
			let path = url.strip_prefix("sqlite://").unwrap_or(url);
			if path.is_empty() || path == ":memory:" {
				tokio_rusqlite::Connection::open_in_memory().await
			} else {
				tokio_rusqlite::Connection::open(path).await
			}
		}
		.map_err(SqliteError::Connect)?;

		conn
			.call(|conn| conn.execute_batch("PRAGMA foreign_keys = ON"))
			.await
			.map_err(SqliteError::Setup)?;
		Ok(SqliteConnection::new(conn))
	}
}

impl ConnectionWithTransaction for SqliteConnection {
	type Transaction<'conn>
		= SqliteTransaction<'conn>
	where
		Self: 'conn;

	async fn transaction<'conn, T, F>(&'conn mut self, f: F) -> Result<T, SqliteError>
	where
		T: 'conn,
		F: for<'tx> AsyncFnOnce(&'tx mut Self::Transaction<'conn>) -> Result<T, SqliteError> + 'conn,
	{
		let mut transaction = SqliteTransaction::new(self.conn.clone());
		begin(&self.conn, &mut transaction).await?;
		let result = f(&mut transaction).await;
		finish_transaction(&self.conn, &mut transaction, result).await
	}

	async fn transaction_scoped<'conn, T, F>(&'conn mut self, f: F) -> Result<T, SqliteError>
	where
		T: Send + 'conn,
		F: for<'tx> FnOnce(
				&'tx mut Self::Transaction<'conn>,
			) -> Pin<Box<dyn Future<Output = Result<T, SqliteError>> + Send + 'tx>>
			+ 'conn,
	{
		let mut transaction = SqliteTransaction::new(self.conn.clone());
		begin(&self.conn, &mut transaction).await?;
		let result = f(&mut transaction).await;
		finish_transaction(&self.conn, &mut transaction, result).await
	}
}

async fn begin(
	conn: &tokio_rusqlite::Connection,
	transaction: &mut SqliteTransaction<'_>,
) -> Result<(), SqliteError> {
	if let Err(error) = conn.call(|conn| conn.execute_batch("BEGIN")).await {
		transaction.finalize();
		return Err(SqliteError::Query(error));
	}
	Ok(())
}

async fn finish_transaction<T>(
	conn: &tokio_rusqlite::Connection,
	transaction: &mut SqliteTransaction<'_>,
	result: Result<T, SqliteError>,
) -> Result<T, SqliteError> {
	match result {
		Ok(value) => {
			conn
				.call(|conn| conn.execute_batch("COMMIT"))
				.await
				.map_err(SqliteError::Query)?;
			transaction.finalize();
			Ok(value)
		}
		Err(error) => {
			if conn
				.call(|conn| conn.execute_batch("ROLLBACK"))
				.await
				.is_ok()
			{
				transaction.finalize();
			}
			Err(error)
		}
	}
}

#[cfg(test)]
mod tests {
	use squealy::Connect;

	use super::{Sqlite, SqliteError};

	#[tokio::test]
	async fn connect_enables_foreign_keys() {
		let connection = Sqlite.connect(":memory:").await.expect("connect");
		let enabled = connection
			.conn
			.call(|conn| conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0)))
			.await
			.expect("read pragma");
		assert_eq!(enabled, 1);
	}

	#[tokio::test]
	async fn execute_batch_runs_all_statements() {
		let connection = Sqlite.connect(":memory:").await.expect("connect");
		connection
			.execute_batch(
				"CREATE TABLE widgets (id INTEGER PRIMARY KEY);\n\
				 INSERT INTO widgets (id) VALUES (1);\n\
				 INSERT INTO widgets (id) VALUES (2);",
			)
			.await
			.expect("execute batch");

		let count = connection
			.conn
			.call(|conn| {
				conn.query_row("SELECT count(*) FROM widgets", [], |row| {
					row.get::<_, i64>(0)
				})
			})
			.await
			.expect("count rows");
		assert_eq!(count, 2);
	}

	#[tokio::test]
	async fn execute_batch_propagates_driver_errors() {
		let connection = Sqlite.connect(":memory:").await.expect("connect");
		let error = connection
			.execute_batch("CREATE TABL broken")
			.await
			.expect_err("invalid SQL must fail");
		assert!(matches!(error, SqliteError::Query(_)));
	}

	#[tokio::test]
	async fn list_user_tables_is_empty_for_a_fresh_database() {
		let connection = Sqlite.connect(":memory:").await.expect("connect");
		assert_eq!(
			connection.list_user_tables().await.expect("list tables"),
			Vec::<String>::new()
		);
	}

	#[tokio::test]
	async fn list_user_tables_filters_internal_tables_and_sorts_by_bytes() {
		let connection = Sqlite.connect(":memory:").await.expect("connect");
		connection
			.execute_batch(
				"CREATE TABLE zed (id INTEGER);\n\
				 CREATE TABLE Alpha (id INTEGER);\n\
				 CREATE TABLE beta (id INTEGER);\n\
				 CREATE TABLE __squealy_metadata (name TEXT);\n\
				 CREATE TABLE identities (id INTEGER PRIMARY KEY AUTOINCREMENT);",
			)
			.await
			.expect("create tables");

		assert_eq!(
			connection.list_user_tables().await.expect("list tables"),
			vec![
				"Alpha".to_owned(),
				"beta".to_owned(),
				"identities".to_owned(),
				"zed".to_owned(),
			]
		);
	}
}
