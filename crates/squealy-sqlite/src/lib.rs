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
//! Introspection ([`SchemaIntrospect`]) reads a live database back into a [`DatabaseModel`] via
//! `sqlite_master` and the PRAGMA table-valued functions (see [`introspect`]), and the backend-owned
//! schema-management bookkeeping ([`SchemaRefactorStore`]/[`SchemaMetadataStore`]/
//! [`SchemaPublishHistoryStore`]) lives in `__squealy_`-prefixed tables (SQLite has no schemas to put
//! them in). Incremental plan rendering ([`SchemaBackend::render_plan`]) applies the changes SQLite's
//! `ALTER TABLE` supports natively (add/drop/rename column, rename table, create/drop index) directly,
//! and every other change — a column type change, or adding/dropping/altering a primary key, unique,
//! foreign key or check, which SQLite carries only inline in `CREATE TABLE` — by rebuilding the whole
//! table (create new, copy, drop, rename). Because dropping a table fires `ON DELETE` actions while
//! foreign keys are enforced, [`DdlExecutor::execute_ddl`] applies a plan with enforcement disabled and
//! re-checks it before committing (SQLite only lets foreign-key enforcement be toggled outside a
//! transaction, so the executor — not the rendered SQL — owns that envelope).

#![forbid(unsafe_code)]

use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;

use squealy::{
    ConnectionWithTransaction, Constraint, DatabaseModel, DatabasePlan, DdlExecutor, DefaultValue,
    ForeignKeyModel, IdentityMode, SchemaBackend, SchemaConnect, SchemaIntrospect,
    SchemaMetadataStore, SchemaPublishHistoryStore, SchemaPublishRecord, SchemaRefactorStore,
    SqlType,
};
use tokio_rusqlite::rusqlite::{self, OptionalExtension};

mod introspect;
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
    #[error("sqlite introspection error: {0}")]
    Introspect(#[source] tokio_rusqlite::Error),
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

    fn render_plan(
        &self,
        plan: &DatabasePlan,
        desired: &DatabaseModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        sql::write_plan(plan, desired, writer)
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
            // A native SQLite URI: open with URI parsing explicitly enabled so its `?`-parameters are
            // honored. (`open` already enables it via `OpenFlags::default`, but spelling out the flag
            // documents the intent and is robust to a future default change.)
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
    ///
    /// Foreign-key enforcement is disabled for the batch and restored afterwards. An incremental plan
    /// rebuilds a changed table by dropping and recreating it, and a `DROP TABLE` fires `ON DELETE`
    /// actions on child rows while enforcement is on (a rebuild of a table with `ON DELETE CASCADE`
    /// children would delete those rows). SQLite only allows `PRAGMA foreign_keys` to be toggled
    /// **outside** a transaction (it is a no-op within one), so enforcement is turned off before `BEGIN`
    /// and back on after the transaction ends; `PRAGMA foreign_key_check` re-validates referential
    /// integrity before the commit, so a genuine violation (e.g. a newly added or tightened foreign key
    /// whose existing data no longer satisfies it) still fails the batch instead of committing silently.
    async fn execute_ddl(&mut self, sql: &str) -> Result<(), SqliteError> {
        let sql = sql.to_owned();
        self.conn
            .call(move |conn| {
                // Toggled outside any transaction (a no-op inside one).
                conn.execute_batch("PRAGMA foreign_keys = OFF")?;
                let result = apply_ddl_batch(conn, &sql);
                // Restore enforcement whatever happened (a failed batch has already rolled back).
                let restored = conn.execute_batch("PRAGMA foreign_keys = ON");
                result.and(restored)
            })
            .await
            .map_err(SqliteError::Execute)
    }
}

/// Applies a DDL batch inside a transaction, re-validating foreign keys before committing. The caller
/// disables enforcement for the batch (see [`DdlExecutor::execute_ddl`]), so referential integrity is
/// not checked statement-by-statement; `PRAGMA foreign_key_check` verifies it before the commit, and
/// any violation aborts the batch (the transaction rolls back on the early return).
fn apply_ddl_batch(conn: &mut rusqlite::Connection, sql: &str) -> rusqlite::Result<()> {
    let transaction = conn.transaction()?;
    transaction.execute_batch(sql)?;
    let has_violation = {
        let mut check = transaction.prepare("PRAGMA foreign_key_check")?;
        let mut rows = check.query([])?;
        rows.next()?.is_some()
    };
    if has_violation {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some("foreign key constraint violation after applying the schema change".to_owned()),
        ));
    }
    transaction.commit()
}

impl SchemaIntrospect for SqliteConnection {
    type Error = SqliteError;

    async fn introspect_database(&mut self) -> Result<DatabaseModel, SqliteError> {
        introspect::database(self).await
    }

    /// SQLite is dynamically typed: a column's declared type only assigns one of five affinities, so a
    /// logical type read back can only be its affinity's representative (e.g. `Varchar`/`Uuid`/`Bool`
    /// all read back as their affinity's type). Collapse a desired type to the same representative so it
    /// does not churn as an ambiguous type change against the live schema.
    fn canonical_sql_type(&self, ty: &SqlType) -> SqlType {
        // A fixed-width byte column keeps its width: SQLite enforces `[u8; N]` with a generated length
        // `CHECK` that introspection recovers as `FixedBytes(N)`, so — unlike other types — it must not
        // collapse to its `BLOB` affinity (`Bytes`), or a width change (or `FixedBytes` vs `Bytes`) would
        // not diff and would leave a stale check.
        if matches!(ty, SqlType::FixedBytes(_)) {
            return ty.clone();
        }
        introspect::representative_type(introspect::affinity_of_sql_type(ty))
    }

    /// SQLite's only identity mechanism is `AUTOINCREMENT`, which introspects back as
    /// [`IdentityMode::AutoIncrement`]. Map every declared mode to it so a crate-declared
    /// `auto_increment` column (which enters the model as `ByDefault`) does not churn as an ambiguous
    /// identity change.
    fn canonical_identity_mode(&self, _mode: &IdentityMode) -> IdentityMode {
        IdentityMode::AutoIncrement
    }

    /// SQLite has no namespaces: every table is rendered and introspected unqualified, so flatten a
    /// desired schema name to `None` to match.
    fn canonical_schema_name(&self, _name: Option<&str>) -> Option<String> {
        None
    }

    /// SQLite has no `CREATE SCHEMA`, so an empty namespace has nothing to create and introspection
    /// reports no schema for an empty database; canonicalization drops an empty flattened schema to
    /// match.
    fn has_namespaces(&self) -> bool {
        false
    }

    /// SQLite does not name a primary key; introspection reports it with an empty name, so flatten a
    /// crate-declared `pk_<table>` to match. A table has at most one primary key, so an empty name never
    /// collides.
    fn canonical_primary_key_name(&self, _name: &str) -> String {
        String::new()
    }

    /// SQLite does not round-trip a `UNIQUE` constraint's name (its backing auto-index is
    /// `sqlite_autoindex_…`). Derive a stable name from the constraint's columns — identical on the
    /// desired and introspected side — so equivalent uniques compare equal while staying distinct when a
    /// table has more than one.
    fn canonical_unique_name(&self, unique: &Constraint) -> String {
        format!("unique:{}", unique.columns.join(","))
    }

    /// SQLite reports foreign keys positionally with no name; derive a stable name from the referencing
    /// columns and the referenced table/columns (identical on both sides) so equivalent foreign keys
    /// compare equal. The referenced schema is already flattened to `None` by
    /// [`canonical_schema_name`](Self::canonical_schema_name), so it is not part of the key.
    fn canonical_foreign_key_name(&self, foreign_key: &ForeignKeyModel) -> String {
        format!(
            "foreign_key:{}->{}({})",
            foreign_key.columns.join(","),
            foreign_key.references_table,
            foreign_key.references_columns.join(","),
        )
    }

    /// SQLite has no boolean or unsigned literal, so a `bool`/unsigned default on an `INTEGER`-affinity
    /// column is rendered as a plain integer and reads back as [`DefaultValue::Int`]; likewise an integer
    /// default on a `REAL`-affinity column reads back as a float. Collapse the desired default the same
    /// way (using the already-canonicalized column type) so a defaulted column does not churn.
    fn canonical_default(&self, ty: &SqlType, default: &DefaultValue) -> DefaultValue {
        match introspect::affinity_of_sql_type(ty) {
            introspect::Affinity::Integer => match default {
                DefaultValue::Bool(value) => DefaultValue::Int(i128::from(*value)),
                // A `UInt` beyond `i128::MAX` (a large `u128`) has no `Int` representation; the renderer
                // writes it as a decimal literal and the introspector — whose `parse::<i128>()` also
                // overflows — reads it back as `Raw(text)`, so canonicalize to that same text.
                DefaultValue::UInt(value) => i128::try_from(*value)
                    .map(DefaultValue::Int)
                    .unwrap_or_else(|_| DefaultValue::Raw(value.to_string())),
                other => other.clone(),
            },
            introspect::Affinity::Real => match default {
                DefaultValue::Int(value) => DefaultValue::Float(*value as f64),
                other => other.clone(),
            },
            // A NUMERIC-affinity column (`Decimal`/`Raw("NUMERIC")`) has no structured numeric literal:
            // the renderer writes the value verbatim and introspection reads it back as `Raw(text)`
            // (SQLite does not preserve the logical numeric type), so collapse a structured numeric
            // default to the same rendered text.
            introspect::Affinity::Numeric => match default {
                DefaultValue::Int(value) => DefaultValue::Raw(value.to_string()),
                DefaultValue::UInt(value) => DefaultValue::Raw(value.to_string()),
                DefaultValue::Float(value) => DefaultValue::Raw(value.to_string()),
                DefaultValue::Bool(value) => {
                    DefaultValue::Raw(if *value { "1" } else { "0" }.to_owned())
                }
                other => other.clone(),
            },
            _ => default.clone(),
        }
    }
}

impl SqliteConnection {
    /// Whether a `__squealy_*` bookkeeping table exists yet (the stores create their table lazily on the
    /// first write, so a read before any write returns empty rather than erroring on a missing table).
    async fn bookkeeping_table_exists(&self, table: &'static str) -> Result<bool, SqliteError> {
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |_| Ok(()),
                )
                .optional()
            })
            .await
            .map(|found| found.is_some())
            .map_err(SqliteError::Introspect)
    }
}

impl SchemaRefactorStore for SqliteConnection {
    type Error = SqliteError;

    async fn applied_refactor_ids(&mut self) -> Result<Vec<String>, SqliteError> {
        if !self.bookkeeping_table_exists("__squealy_refactors").await? {
            return Ok(Vec::new());
        }
        self.conn
            .call(|conn| {
                let mut statement =
                    conn.prepare("SELECT id FROM __squealy_refactors ORDER BY id")?;
                let ids = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(ids)
            })
            .await
            .map_err(SqliteError::Introspect)
    }

    async fn record_applied_refactor_ids(&mut self, ids: &[String]) -> Result<(), SqliteError> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids = ids.to_vec();
        self.conn
            .call(move |conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS __squealy_refactors (\
                     id TEXT NOT NULL PRIMARY KEY, \
                     applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
                )?;
                let transaction = conn.transaction()?;
                {
                    let mut statement = transaction
                        .prepare("INSERT OR IGNORE INTO __squealy_refactors (id) VALUES (?1)")?;
                    for id in &ids {
                        statement.execute([id])?;
                    }
                }
                transaction.commit()
            })
            .await
            .map_err(SqliteError::Execute)
    }
}

impl SchemaMetadataStore for SqliteConnection {
    type Error = SqliteError;

    async fn schema_metadata(&mut self) -> Result<Vec<(String, String)>, SqliteError> {
        if !self.bookkeeping_table_exists("__squealy_metadata").await? {
            return Ok(Vec::new());
        }
        self.conn
            .call(|conn| {
                let mut statement =
                    conn.prepare("SELECT name, value FROM __squealy_metadata ORDER BY name")?;
                let entries = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(entries)
            })
            .await
            .map_err(SqliteError::Introspect)
    }

    async fn record_schema_metadata(
        &mut self,
        entries: &[(String, String)],
    ) -> Result<(), SqliteError> {
        if entries.is_empty() {
            return Ok(());
        }
        let entries = entries.to_vec();
        self.conn
            .call(move |conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS __squealy_metadata (\
                     name TEXT NOT NULL PRIMARY KEY, \
                     value TEXT NOT NULL, \
                     updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
                )?;
                let transaction = conn.transaction()?;
                {
                    let mut statement = transaction.prepare(
                        "INSERT INTO __squealy_metadata (name, value) VALUES (?1, ?2) \
                         ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                    )?;
                    for (name, value) in &entries {
                        statement.execute(rusqlite::params![name, value])?;
                    }
                }
                transaction.commit()
            })
            .await
            .map_err(SqliteError::Execute)
    }
}

impl SchemaPublishHistoryStore for SqliteConnection {
    type Error = SqliteError;

    async fn schema_publish_history(
        &mut self,
        limit: usize,
    ) -> Result<Vec<SchemaPublishRecord>, SqliteError> {
        if limit == 0
            || !self
                .bookkeeping_table_exists("__squealy_publish_history")
                .await?
        {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.conn
            .call(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT mode, package_hash, package_format_version, \
                            strftime('%Y-%m-%dT%H:%M:%S', applied_at) \
                     FROM __squealy_publish_history ORDER BY id DESC LIMIT ?1",
                )?;
                let records = statement
                    .query_map([limit], |row| {
                        Ok(SchemaPublishRecord {
                            mode: row.get(0)?,
                            package_hash: row.get(1)?,
                            package_format_version: row.get(2)?,
                            applied_at: row.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(records)
            })
            .await
            .map_err(SqliteError::Introspect)
    }

    async fn record_schema_publish(
        &mut self,
        mode: &str,
        package_hash: &str,
        package_format_version: &str,
    ) -> Result<(), SqliteError> {
        let (mode, package_hash, package_format_version) = (
            mode.to_owned(),
            package_hash.to_owned(),
            package_format_version.to_owned(),
        );
        self.conn
            .call(move |conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS __squealy_publish_history (\
                     id INTEGER PRIMARY KEY AUTOINCREMENT, \
                     mode TEXT NOT NULL, \
                     package_hash TEXT NOT NULL, \
                     package_format_version TEXT NOT NULL, \
                     applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
                )?;
                conn.execute(
                    "INSERT INTO __squealy_publish_history \
                     (mode, package_hash, package_format_version) VALUES (?1, ?2, ?3)",
                    rusqlite::params![mode, package_hash, package_format_version],
                )?;
                Ok(())
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
        begin(&self.conn, &mut transaction).await?;
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
        begin(&self.conn, &mut transaction).await?;
        let result = f(&mut transaction).await;
        finish_transaction(&self.conn, &mut transaction, result).await
    }
}

/// Issues `BEGIN`. On failure — e.g. the connection is already inside a transaction (another
/// `SqliteConnection` over a clone of the same handle started one) — the guard is disarmed before the
/// error propagates, so its drop does not roll back a transaction this call never started. If instead
/// the future is *cancelled* while `BEGIN` is in flight, the guard stays armed and rolls back any
/// transaction that did begin.
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
