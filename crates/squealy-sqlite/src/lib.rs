//! SQLite backend for squealy.
//!
//! Renders the SQLite dialect (double-quoted identifiers, type affinities
//! (`INTEGER`/`REAL`/`TEXT`/`BLOB`/`NUMERIC`), `INTEGER PRIMARY KEY AUTOINCREMENT` identity, and
//! **inline** foreign keys — SQLite cannot `ALTER TABLE … ADD CONSTRAINT`) for both schema management
//! (DDL rendering against the core [`DatabaseModel`]) and query execution. The query runtime — the
//! value codec, [`Backend`](squealy::Backend), and the executable query objects — lives in [`query`]
//! and runs against a live database through [`tokio-rusqlite`](tokio_rusqlite), which owns the
//! (`!Send`) `rusqlite::Connection` on a dedicated thread and hands it closures over a channel.
//!
//! Introspection and incremental plan rendering ([`SchemaBackend::render_plan`]) are not yet
//! supported: SQLite's `ALTER TABLE` only adds/drops/renames columns and renames tables, so most
//! changes need the "create new table, copy, drop, rename" rebuild — a future slice.

#![forbid(unsafe_code)]

use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;

use squealy::{
    ConnectionWithTransaction, DatabaseModel, DatabasePlan, DdlExecutor, SchemaBackend,
    SchemaConnect,
};

mod query;
mod sql;

pub use query::{SqliteRowReader, SqliteTransaction, SqliteValue};

/// The SQLite schema/query backend marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sqlite;

/// An error connecting to, executing DDL against, decoding a result column, encoding a bind
/// parameter, or querying SQLite.
#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
    #[error("sqlite connect error: {0}")]
    Connect(#[source] tokio_rusqlite::rusqlite::Error),
    #[error("sqlite ddl error: {0}")]
    Execute(#[source] tokio_rusqlite::Error),
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
}

impl SchemaBackend for Sqlite {
    fn capabilities(&self) -> squealy::SchemaCapabilities {
        // Mirrors what the renderer accepts: SQLite supports partial (predicate) indexes, but none of
        // the other index metadata, and no constraint validation/enforcement/deferrability/match
        // metadata. Without advertising `predicates`, the schema engine's `check_create` would reject a
        // partial index before this backend ever rendered it.
        squealy::SchemaCapabilities {
            constraints: squealy::ConstraintCapabilities::default(),
            indexes: squealy::IndexCapabilities {
                predicates: true,
                ..squealy::IndexCapabilities::default()
            },
        }
    }

    fn render_create(&self, model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
        sql::write_database(model, writer)
    }

    fn render_plan(&self, _plan: &DatabasePlan, _writer: &mut impl Write) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite incremental schema plan rendering is not supported yet: SQLite's ALTER TABLE \
             cannot change a column's type or add/drop most constraints, so changes require a \
             create-copy-drop-rename table rebuild (a future slice)",
        ))
    }
}

/// A live SQLite connection for schema management and query execution.
///
/// Wraps a [`tokio_rusqlite::Connection`], a cheap [`Clone`] handle that owns the underlying
/// (`!Send`) `rusqlite::Connection` on a dedicated thread and runs every closure submitted through
/// [`call`](tokio_rusqlite::Connection::call) FIFO on that thread. Query execution therefore needs
/// only `&self` (no `Mutex`, unlike the MySQL backend): the handle serializes access itself.
pub struct SqliteConnection {
    pub(crate) conn: tokio_rusqlite::Connection,
}

impl SqliteConnection {
    /// Wraps an already-established [`tokio_rusqlite::Connection`].
    ///
    /// This does not configure the connection. [`Sqlite::connect`](SchemaConnect::connect) enables
    /// foreign-key enforcement (`PRAGMA foreign_keys = ON`), which this backend's **inline** foreign
    /// keys rely on; a connection built directly here should run that pragma before relying on it.
    pub fn new(conn: tokio_rusqlite::Connection) -> Self {
        Self { conn }
    }
}

impl std::fmt::Debug for SqliteConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SqliteConnection").finish()
    }
}

impl SchemaConnect for Sqlite {
    type Connection = SqliteConnection;
    type Error = SqliteError;

    /// Opens a SQLite database. `url` may be:
    /// - a native SQLite `file:` URI (e.g. `file:app.db?mode=ro`, `file::memory:?cache=shared`), passed
    ///   through verbatim so SQLite parses its parameters (rusqlite enables URI parsing by default);
    /// - `":memory:"` (or an empty string, or `sqlite://`) for a private in-memory database;
    /// - otherwise a filesystem path, with an optional `sqlite://` convenience prefix stripped.
    ///
    /// Foreign-key enforcement is enabled on the fresh connection (`PRAGMA foreign_keys = ON`) — it is
    /// off by default in SQLite and must be set outside any transaction — so the inline foreign keys
    /// this backend renders are enforced at runtime.
    async fn connect(&self, url: &str) -> Result<SqliteConnection, SqliteError> {
        let conn = if url.starts_with("file:") {
            // A native SQLite URI: pass it through unchanged so its `?`-parameters are honored.
            tokio_rusqlite::Connection::open(url).await
        } else {
            let path = url.strip_prefix("sqlite://").unwrap_or(url);
            if path.is_empty() || path == ":memory:" {
                tokio_rusqlite::Connection::open_in_memory().await
            } else {
                tokio_rusqlite::Connection::open(path).await
            }
        }
        .map_err(SqliteError::Connect)?;
        // Foreign keys are off by default and the setting is a no-op inside a transaction, so enable it
        // now on the idle connection. `execute_batch` (not `execute`) because a `PRAGMA` returns no rows.
        conn.call(|conn| conn.execute_batch("PRAGMA foreign_keys = ON"))
            .await
            .map_err(SqliteError::Execute)?;
        Ok(SqliteConnection::new(conn))
    }
}

impl DdlExecutor for SqliteConnection {
    type Error = SqliteError;

    /// Runs the whole DDL batch **atomically** inside a transaction. Unlike MySQL, SQLite has
    /// transactional DDL, so a mid-batch failure rolls the whole batch back — there is no
    /// partially-applied-schema state to report.
    async fn execute_ddl(&mut self, sql: &str) -> Result<(), SqliteError> {
        let sql = sql.to_owned();
        self.conn
            .call(move |conn| {
                let transaction = conn.transaction()?;
                transaction.execute_batch(&sql)?;
                transaction.commit()
            })
            .await
            .map_err(SqliteError::Execute)
    }
}

impl ConnectionWithTransaction for SqliteConnection {
    type Transaction<'conn>
        = SqliteTransaction<'conn>
    where
        Self: 'conn;

    // Both entry points share the same `BEGIN` / body / finish shape; they differ only in how the body
    // closure is called (`AsyncFnOnce` vs a boxed-future `FnOnce`), and the body future borrows the
    // transaction, so the two cannot be unified behind one generic helper. The transaction handle owns
    // its own clone of the connection (a handle to the *same* underlying database), so `&mut self` is
    // not frozen for the duration; correctness rests on `tokio-rusqlite` running every submitted closure
    // FIFO on the one owned thread, and on this method holding the connection exclusively.
    //
    // The drop-guard transaction is constructed *before* `BEGIN`: if the future is cancelled while
    // `BEGIN` is in flight, the guard still exists and rolls back (a no-op if `BEGIN` never ran). The
    // guard is only disarmed (via `finish_transaction`) after a `COMMIT`/`ROLLBACK` actually succeeds,
    // so a failed `COMMIT` leaves it armed to clean up.
    async fn transaction<'conn, T, F>(&'conn mut self, f: F) -> Result<T, SqliteError>
    where
        T: 'conn,
        F: for<'tx> AsyncFnOnce(&'tx mut Self::Transaction<'conn>) -> Result<T, SqliteError>
            + 'conn,
    {
        let mut transaction = SqliteTransaction::new(self.conn.clone());
        self.conn
            .call(|conn| conn.execute_batch("BEGIN"))
            .await
            .map_err(SqliteError::Query)?;
        let result = f(&mut transaction).await;
        finish_transaction(&self.conn, &mut transaction, result).await
    }

    async fn transaction_scoped<'conn, T, F>(&'conn mut self, f: F) -> Result<T, SqliteError>
    where
        T: Send + 'conn,
        F: for<'tx> FnOnce(
                &'tx mut Self::Transaction<'conn>,
            )
                -> Pin<Box<dyn Future<Output = Result<T, SqliteError>> + Send + 'tx>>
            + 'conn,
    {
        let mut transaction = SqliteTransaction::new(self.conn.clone());
        self.conn
            .call(|conn| conn.execute_batch("BEGIN"))
            .await
            .map_err(SqliteError::Query)?;
        let result = f(&mut transaction).await;
        finish_transaction(&self.conn, &mut transaction, result).await
    }
}

/// Commits (on `Ok`) or rolls back (on `Err`) and propagates the body's result, disarming the
/// transaction's drop guard **only** when the `COMMIT`/`ROLLBACK` actually succeeds:
/// - A failed `COMMIT` (e.g. `SQLITE_BUSY`, which can leave the transaction open) returns the commit
///   error with the guard still armed, so the drop guard rolls back.
/// - A failed `ROLLBACK` keeps the original body error and leaves the guard armed to retry on drop.
async fn finish_transaction<T>(
    conn: &tokio_rusqlite::Connection,
    transaction: &mut SqliteTransaction<'_>,
    result: Result<T, SqliteError>,
) -> Result<T, SqliteError> {
    match result {
        Ok(value) => {
            conn.call(|conn| conn.execute_batch("COMMIT"))
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
