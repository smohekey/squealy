//! MySQL query backend for `squealy`.
//!
//! The driver connection is held behind a mutex because `mysql_async` requires
//! mutable access while squealy's execution traits operate through shared references.

#![forbid(unsafe_code)]

use std::fmt;

#[cfg(any(feature = "time", feature = "chrono", feature = "systemtime"))]
use mysql_async::prelude::Queryable;

mod query;
mod sql;

#[cfg(feature = "serde")]
pub use query::Json;
pub use query::MysqlRowReader;

/// The MySQL query backend marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mysql;

impl Mysql {
	/// This backend's SQL query dialect.
	pub fn dialect(&self) -> impl squealy::Dialect {
		crate::sql::MysqlDialect
	}
}

impl squealy::SupportsIntersectExceptAll for Mysql {}
impl squealy::SupportsColumnlessUpsert for Mysql {}
impl squealy::SupportsDefaultKeyword for Mysql {}
impl squealy::SupportsExtract for Mysql {}
impl squealy::SupportsNamedWindow for Mysql {}

/// A live MySQL connection.
pub struct MysqlConnection {
	conn: tokio::sync::Mutex<mysql_async::Conn>,
}

impl MysqlConnection {
	/// Wraps an existing driver connection without changing its session settings.
	///
	/// Callers using timestamp codecs must configure the session for UTC themselves.
	/// [`Mysql::connect`](squealy::Connect::connect) performs that setup automatically.
	pub fn new(conn: mysql_async::Conn) -> Self {
		Self {
			conn: tokio::sync::Mutex::new(conn),
		}
	}

	pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, mysql_async::Conn> {
		self.conn.lock().await
	}
}

impl fmt::Debug for MysqlConnection {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.debug_struct("MysqlConnection").finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub enum MysqlError {
	#[error("mysql connect error: {0}")]
	Connect(#[source] mysql_async::Error),
	#[error("mysql query error: {0}")]
	Query(#[source] mysql_async::Error),
	#[error("query returned no rows")]
	NoRows,
	#[error("row is missing column {0}")]
	MissingColumn(usize),
	#[error("could not decode column {column}: {source}")]
	Decode {
		column: usize,
		#[source]
		source: mysql_async::FromValueError,
	},
	#[error("could not convert value to {0}")]
	Conversion(&'static str),
	#[error("mysql render error: {0}")]
	Render(#[source] std::io::Error),
}

impl squealy::Connect for Mysql {
	type Connection = MysqlConnection;
	type Error = MysqlError;

	async fn connect(&self, url: &str) -> Result<MysqlConnection, MysqlError> {
		let conn = mysql_async::Conn::from_url(url)
			.await
			.map_err(MysqlError::Connect)?;

		#[cfg(any(feature = "time", feature = "chrono", feature = "systemtime"))]
		let conn = {
			let mut conn = conn;
			conn
				.query_drop("SET time_zone = '+00:00'")
				.await
				.map_err(MysqlError::Connect)?;
			conn
		};

		Ok(MysqlConnection::new(conn))
	}
}
