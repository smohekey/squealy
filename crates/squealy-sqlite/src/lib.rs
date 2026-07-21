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

	use super::Sqlite;

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
}
