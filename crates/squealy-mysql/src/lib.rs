//! MySQL backend for squealy.
//!
//! Renders the MySQL dialect (backtick quoting, `AUTO_INCREMENT` identity, unsigned integers,
//! `VARCHAR`-backed strings) for both schema management (DDL/introspection against the core
//! `DatabaseModel`) and query execution. The query runtime lives in [`query`]; the single driver
//! `Conn` is held behind a [`tokio::sync::Mutex`] so the `&self` execution API can obtain the
//! `&mut Conn` that `mysql_async` requires.
//!
//! # MySQL backend: differences & limitations
//!
//! A consolidated, user-facing reference for choosing and operating this backend. Where PostgreSQL
//! and MySQL legitimately differ, the difference is expressed through the `Dialect`/`Backend` seams
//! rather than leaking into the core model, so most divergences are invisible at the call site. The
//! points below are the ones worth knowing.
//!
//! ## Type codecs (feature-gated)
//!
//! Each optional codec is behind a Cargo feature and encodes/decodes against one column type:
//!
//! - **`uuid`** — `uuid::Uuid` against `CHAR(36)` (hyphenated lowercase text). A bare `Uuid` column
//!   canonicalizes to `Char(36)`, so it does not churn a schema diff on re-publish.
//! - **`serde`** — `Json<T>` (the wrapper is defined in this crate) for any
//!   `T: Serialize + DeserializeOwned`, against a `JSON` column via `serde_json`. No core feature is
//!   needed — only the serde crates.
//! - **`time`**, **`chrono`**, **`systemtime`** — `time::OffsetDateTime`, `chrono::DateTime<Utc>`, and
//!   `std::time::SystemTime` respectively, each against a bare `TIMESTAMP` column.
//! - **`bytes`** — `bytes::Bytes` against a `BLOB` column.
//!
//! ## Timestamps: UTC, whole-second, 1970–2038
//!
//! The datetime codecs store and return **UTC** values at **whole-second** resolution (a bare
//! `TIMESTAMP`, fractional-seconds precision 0). Sub-second precision needs the neutral model to carry
//! fractional precision and is planned, not yet supported.
//!
//! Because MySQL interprets a `TIMESTAMP` in the session time zone, the session must be UTC:
//!
//! - `Mysql::connect` (the `SchemaConnect` impl) runs `SET time_zone = '+00:00'` for you — but only
//!   when a timestamp codec feature (`time`/`chrono`/`systemtime`) is compiled in.
//! - `MysqlConnection::new`, which adopts an already-open `mysql_async::Conn`, does **not** touch the
//!   session. A caller using it with the timestamp codecs must run `SET time_zone = '+00:00'` itself,
//!   or stored instants shift by the session offset.
//!
//! Instants outside MySQL's `TIMESTAMP` range — `1970-01-01 00:00:01` through
//! `2038-01-19 03:14:07` UTC (Unix seconds `1..=2_147_483_647`; the zero/epoch timestamp is *not*
//! accepted) — are rejected at bind time rather than silently wrapped.
//!
//! ## Compile-time-gated PostgreSQL-only features
//!
//! A few PostgreSQL query features are gated behind marker traits that `Mysql` does not implement, so
//! code using them **fails to compile** against a MySQL connection — there is no silent runtime
//! fallback:
//!
//! - `RETURNING` (`SupportsReturning`)
//! - `FULL JOIN` (`SupportsFullJoin`)
//! - `date_trunc` / `AT TIME ZONE` (`SupportsDateTrunc`)
//!
//! MySQL *does* implement the other capability traits: `EXTRACT` (`SupportsExtract`),
//! `INTERSECT ALL` / `EXCEPT ALL` (`SupportsIntersectExceptAll`, MySQL 8.0.31+), columnless upsert
//! (`SupportsColumnlessUpsert`), and the `DEFAULT` keyword as an assignment value
//! (`SupportsDefaultKeyword`).
//!
//! ## Cleanly rejected DDL
//!
//! Index shapes MySQL cannot express are rejected with an `io::Error` at render time (never silently
//! dropped):
//!
//! - **partial / filtered indexes** (a `where = ...` predicate on a `#[unique]`/`#[index]`) — e.g.
//!   *"MySQL does not support partial index predicates"* / *"MySQL does not support partial (filtered)
//!   unique indexes"*.
//! - **expression indexes** — *"MySQL expression indexes are not supported by squealy yet"*.
//!
//! (PostgreSQL supports both; SQLite renders partial indexes but not expression indexes. These
//! rejections are specific to the MySQL backend.)
//!
//! ## Prepared statements
//!
//! Squealy's prepared-statement API (`prepare()`) — and `RETURNING` — are **intentionally not
//! implemented** for MySQL: only the directly-executable query forms are provided. This is *not* an
//! inlining/safety difference — one-shot `fetch`/`execute` still render `?` placeholders and bind their
//! values positionally through the driver (`mysql_async` `Params::Positional`); what is absent is the
//! reusable prepared-query object, so a query built with runtime bind slots is rejected on this backend.
//!
//! ## Schema-diff expression fidelity
//!
//! MySQL has no expression-canonicalization pass (there is no `canonical.rs`, unlike the PostgreSQL
//! backend). It inherits the identity `canonical_check_expression` / `canonical_index_predicate`, so a
//! `CHECK` or partial-index predicate that the server's catalog stores in a re-spelled form can diff as
//! a spurious change. Tightening this is planned.
//!
//! ## Dialect divergences (transparent)
//!
//! These behave correctly but render differently from PostgreSQL; the dialect seam handles them, so no
//! call-site change is needed:
//!
//! - `NULLS FIRST/LAST` in an executable query `ORDER BY` is emulated with a leading `(<expr> IS NULL)`
//!   sort key. In **view DDL**, by contrast, the explicit modifier is dropped, so a view carrying
//!   `OrderNulls::First`/`Last` renders with MySQL's default NULL ordering rather than emulation.
//! - `FOR SHARE` renders as `LOCK IN SHARE MODE`.
//! - case-insensitive `LIKE` (`ILIKE`) relies on MySQL's default case-insensitive collation (plain
//!   `LIKE`).
//! - string concatenation uses `CONCAT(...)`, not `||`.
//! - integer `/` is already float division (MySQL spells integer division `DIV`), so no float-cast
//!   wrapping is emitted.
//! - the fractional-seconds `extract_second(...)` helper uses the composite `SECOND_MICROSECOND` unit
//!   (a plain `extract(Second, ...)` still renders `EXTRACT(SECOND FROM ...)`, whole seconds).
//! - `UPDATE ... FROM` and `DELETE ... USING` render as MySQL multi-table `JOIN` forms.
//! - upsert renders as `ON DUPLICATE KEY UPDATE`, with `VALUES(col)` for an excluded value.

#![forbid(unsafe_code)]

use std::fmt;
use std::io::Write;

use mysql_async::prelude::Queryable;
use squealy::{
    DatabaseModel, DdlExecutor, SchemaBackend, SchemaConnect, SchemaIntrospect,
    SchemaMetadataStore, SchemaPublishHistoryStore, SchemaPublishRecord, SchemaRefactorStore,
};

mod introspect;
mod query;
mod sql;

#[cfg(feature = "serde")]
pub use query::Json;
pub use query::MysqlRowReader;

/// The MySQL schema backend marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mysql;

// MySQL (8.0.31+) supports `INTERSECT ALL` / `EXCEPT ALL`.
impl squealy::SupportsIntersectExceptAll for Mysql {}

// MySQL renders a columnless upsert via `() VALUES () ON DUPLICATE KEY UPDATE` (self-assigning the
// conflict-target column), so an all-default-row upsert is expressible.
impl squealy::SupportsColumnlessUpsert for Mysql {}

// MySQL accepts the `DEFAULT` keyword as an assignment value (`VALUES (DEFAULT)`, `SET c = DEFAULT`).
impl squealy::SupportsDefaultKeyword for Mysql {}

// MySQL supports `EXTRACT(<field> FROM <ts>)` (and the `SECOND_MICROSECOND` unit for fractional seconds).
impl squealy::SupportsExtract for Mysql {}

impl SchemaBackend for Mysql {
    fn capabilities(&self) -> squealy::SchemaCapabilities {
        squealy::SchemaCapabilities {
            constraints: squealy::ConstraintCapabilities::default(),
            indexes: squealy::IndexCapabilities::default(),
        }
    }

    fn render_create(&self, model: &DatabaseModel, writer: &mut impl Write) -> std::io::Result<()> {
        sql::write_database(model, writer)
    }

    fn render_plan(
        &self,
        plan: &squealy::DatabasePlan,
        // MySQL renders each step's delta in place (`ALTER TABLE … MODIFY COLUMN …`), so it does not
        // need the full target model that table-rebuild backends (SQLite) rely on.
        _desired: &DatabaseModel,
        writer: &mut impl Write,
    ) -> std::io::Result<()> {
        sql::write_plan(plan, writer)
    }
}

/// A live MySQL connection for schema management and query execution.
///
/// The driver `Conn` is held behind a [`tokio::sync::Mutex`] so query execution — which the core API
/// drives through `&self` — can borrow the `&mut Conn` that `mysql_async` requires. Schema operations
/// already take `&mut self` and reach the connection through [`get_mut`](tokio::sync::Mutex::get_mut)
/// without locking. A single connection runs one statement at a time, so the lock is the honest model
/// rather than a compromise.
pub struct MysqlConnection {
    conn: tokio::sync::Mutex<mysql_async::Conn>,
}

impl MysqlConnection {
    /// Wraps an already-established `mysql_async::Conn`.
    ///
    /// This does not configure the session (it is synchronous and issues no SQL). If you use the
    /// timestamp codecs (the `time`/`chrono`/`systemtime` features), the session **must** run with
    /// `time_zone = '+00:00'`: those codecs bind/return UTC values, and MySQL interprets a `TIMESTAMP`
    /// in the session zone, so a non-UTC session shifts stored instants. Run `SET time_zone = '+00:00'`
    /// on the connection before binding timestamp values, or use `Mysql::connect` (the `SchemaConnect`
    /// impl), which establishes it for you.
    pub fn new(conn: mysql_async::Conn) -> Self {
        Self {
            conn: tokio::sync::Mutex::new(conn),
        }
    }

    /// Borrows the underlying connection for a schema operation that already holds `&mut self`.
    fn conn_mut(&mut self) -> &mut mysql_async::Conn {
        self.conn.get_mut()
    }

    /// Locks the connection for a query driven through the shared `&self` execution API. The guard is
    /// held for the duration of one statement (a connection runs one at a time).
    pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, mysql_async::Conn> {
        self.conn.lock().await
    }
}

impl fmt::Debug for MysqlConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MysqlConnection").finish()
    }
}

/// An error connecting to, executing DDL against, or querying MySQL.
#[derive(Debug, thiserror::Error)]
pub enum MysqlError {
    #[error("mysql connect error: {0}")]
    Connect(#[source] mysql_async::Error),
    #[error("mysql ddl error: {0}")]
    Execute(#[source] mysql_async::Error),
    #[error("mysql introspection error: {0}")]
    Introspect(#[source] mysql_async::Error),
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
    /// A statement in a multi-statement DDL batch failed. MySQL auto-commits DDL, so the
    /// `applied` statements before it are already committed and were not rolled back.
    #[error(
        "mysql ddl error after applying {applied} of {total} statement(s): MySQL auto-commits DDL, \
         so those {applied} statement(s) are already committed and were not rolled back — the \
         schema is partially applied and may need manual inspection before retrying. Failed on \
         `{statement}`: {source}"
    )]
    PartialDdl {
        applied: usize,
        total: usize,
        statement: String,
        #[source]
        source: mysql_async::Error,
    },
}

impl SchemaConnect for Mysql {
    type Connection = MysqlConnection;
    type Error = MysqlError;

    async fn connect(&self, url: &str) -> Result<MysqlConnection, MysqlError> {
        let conn = mysql_async::Conn::from_url(url)
            .await
            .map_err(MysqlError::Connect)?;
        // The timestamp codecs bind (and read back) UTC civil values. MySQL interprets a `TIMESTAMP`
        // literal in the session zone, so on a non-UTC session a UTC value would be shifted on write;
        // pin the session to UTC so stored instants round-trip. Only relevant when a timestamp codec
        // is compiled in, so the connection behaviour is otherwise unchanged.
        #[cfg(any(feature = "time", feature = "chrono", feature = "systemtime"))]
        let conn = {
            let mut conn = conn;
            conn.query_drop("SET time_zone = '+00:00'")
                .await
                .map_err(MysqlError::Connect)?;
            conn
        };
        Ok(MysqlConnection::new(conn))
    }
}

impl DdlExecutor for MysqlConnection {
    type Error = MysqlError;

    /// Runs the DDL batch one statement at a time.
    ///
    /// MySQL has **no transactional DDL** — each `CREATE`/`ALTER` auto-commits — so unlike the
    /// PostgreSQL backend this is *not* atomic: a mid-batch failure leaves earlier statements
    /// applied. A failure is therefore reported as [`MysqlError::PartialDdl`], which records how
    /// many statements were already committed and which statement failed so the operator can
    /// recover the partially-applied schema.
    async fn execute_ddl(&mut self, sql: &str) -> Result<(), MysqlError> {
        let statements = split_statements(sql);
        let total = statements.len();
        for (index, statement) in statements.into_iter().enumerate() {
            self.conn_mut()
                .query_drop(statement)
                .await
                .map_err(|source| MysqlError::PartialDdl {
                    applied: index,
                    total,
                    statement: statement.to_owned(),
                    source,
                })?;
        }
        Ok(())
    }
}

impl SchemaIntrospect for MysqlConnection {
    type Error = MysqlError;

    async fn introspect_database(&mut self) -> Result<DatabaseModel, MysqlError> {
        introspect::database(self.conn_mut()).await
    }

    /// MySQL renders bare `String` as `VARCHAR(255)` (it has no key-usable unbounded `text`), which
    /// introspects back as `Varchar(255)`; map `String` to that physical form so a desired model
    /// using `String` does not churn as an ambiguous type change against the live schema.
    fn canonical_sql_type(&self, ty: &squealy::SqlType) -> squealy::SqlType {
        canonical_sql_type(ty)
    }

    /// MySQL has only `AUTO_INCREMENT`: it renders any identity column that way and introspects it
    /// back as [`IdentityMode::AutoIncrement`](squealy::IdentityMode::AutoIncrement). Map every mode
    /// to that so a crate-declared `auto_increment` column (which enters the model as `ByDefault`)
    /// does not churn as an ambiguous identity change against the live schema.
    fn canonical_identity_mode(&self, _mode: &squealy::IdentityMode) -> squealy::IdentityMode {
        squealy::IdentityMode::AutoIncrement
    }

    /// MySQL introspection reports a plain index's access method as `BTREE`; map an unset method to
    /// that so a crate-declared index does not churn against the live schema.
    fn default_index_method(&self) -> Option<squealy::IndexMethod> {
        Some(squealy::IndexMethod::BTree)
    }

    /// MySQL ignores a declared primary-key constraint name and always reports it as `PRIMARY`; map
    /// the desired name to that so a crate-declared `pk_<table>` does not churn against the live
    /// schema.
    fn canonical_primary_key_name(&self, _name: &str) -> String {
        "PRIMARY".to_owned()
    }
}

/// Maps a neutral [`SqlType`](squealy::SqlType) to the physical form the MySQL introspector reads
/// back, so a desired model does not churn against a live schema:
/// - MySQL has no key-usable unbounded `text`, so bare `String` is rendered (and read back) as
///   `VARCHAR(255)`.
/// - MySQL has no native `uuid` type: a `uuid::Uuid` column is rendered as `CHAR(36)`, which
///   introspects back as `Char(36)`.
fn canonical_sql_type(ty: &squealy::SqlType) -> squealy::SqlType {
    match ty {
        squealy::SqlType::String => squealy::SqlType::Varchar(255),
        squealy::SqlType::Uuid => squealy::SqlType::Char(36),
        other => other.clone(),
    }
}

impl SchemaRefactorStore for MysqlConnection {
    type Error = MysqlError;

    async fn applied_refactor_ids(&mut self) -> Result<Vec<String>, MysqlError> {
        let exists = self
            .conn_mut()
            .query_first::<u8, _>(
                "\
SELECT 1
FROM information_schema.TABLES
WHERE TABLE_SCHEMA = '__squealy'
  AND TABLE_NAME = 'refactors'
LIMIT 1",
            )
            .await
            .map_err(MysqlError::Introspect)?
            .is_some();
        if !exists {
            return Ok(Vec::new());
        }

        self.conn_mut()
            .query_map(
                "SELECT `id` FROM `__squealy`.`refactors` ORDER BY `id`",
                |id| id,
            )
            .await
            .map_err(MysqlError::Introspect)
    }

    async fn record_applied_refactor_ids(&mut self, ids: &[String]) -> Result<(), MysqlError> {
        if ids.is_empty() {
            return Ok(());
        }

        self.conn_mut()
            .query_drop("CREATE SCHEMA IF NOT EXISTS `__squealy`")
            .await
            .map_err(MysqlError::Execute)?;
        self.conn_mut()
            .query_drop(
                "\
CREATE TABLE IF NOT EXISTS `__squealy`.`refactors` (
    `id` VARCHAR(255) NOT NULL PRIMARY KEY,
    `applied_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
)",
            )
            .await
            .map_err(MysqlError::Execute)?;

        for id in ids {
            self.conn_mut()
                .exec_drop(
                    "INSERT IGNORE INTO `__squealy`.`refactors` (`id`) VALUES (?)",
                    (id.as_str(),),
                )
                .await
                .map_err(MysqlError::Execute)?;
        }

        Ok(())
    }
}

impl SchemaMetadataStore for MysqlConnection {
    type Error = MysqlError;

    async fn schema_metadata(&mut self) -> Result<Vec<(String, String)>, MysqlError> {
        let exists = self
            .conn_mut()
            .query_first::<u8, _>(
                "\
SELECT 1
FROM information_schema.TABLES
WHERE TABLE_SCHEMA = '__squealy'
  AND TABLE_NAME = 'metadata'
LIMIT 1",
            )
            .await
            .map_err(MysqlError::Introspect)?
            .is_some();
        if !exists {
            return Ok(Vec::new());
        }

        self.conn_mut()
            .query_map(
                "SELECT `name`, `value` FROM `__squealy`.`metadata` ORDER BY `name`",
                |(name, value)| (name, value),
            )
            .await
            .map_err(MysqlError::Introspect)
    }

    async fn record_schema_metadata(
        &mut self,
        entries: &[(String, String)],
    ) -> Result<(), MysqlError> {
        if entries.is_empty() {
            return Ok(());
        }

        self.conn_mut()
            .query_drop("CREATE SCHEMA IF NOT EXISTS `__squealy`")
            .await
            .map_err(MysqlError::Execute)?;
        self.conn_mut()
            .query_drop(
                "\
CREATE TABLE IF NOT EXISTS `__squealy`.`metadata` (
    `name` VARCHAR(255) NOT NULL PRIMARY KEY,
    `value` TEXT NOT NULL,
    `updated_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
)",
            )
            .await
            .map_err(MysqlError::Execute)?;

        for (name, value) in entries {
            self.conn_mut()
                .exec_drop(
                    "\
INSERT INTO `__squealy`.`metadata` (`name`, `value`)
VALUES (?, ?)
ON DUPLICATE KEY UPDATE `value` = VALUES(`value`)",
                    (name.as_str(), value.as_str()),
                )
                .await
                .map_err(MysqlError::Execute)?;
        }

        Ok(())
    }
}

impl SchemaPublishHistoryStore for MysqlConnection {
    type Error = MysqlError;

    async fn schema_publish_history(
        &mut self,
        limit: usize,
    ) -> Result<Vec<SchemaPublishRecord>, MysqlError> {
        let exists = self
            .conn_mut()
            .query_first::<u8, _>(
                "\
SELECT 1
FROM information_schema.TABLES
WHERE TABLE_SCHEMA = '__squealy'
  AND TABLE_NAME = 'publish_history'
LIMIT 1",
            )
            .await
            .map_err(MysqlError::Introspect)?
            .is_some();
        if !exists || limit == 0 {
            return Ok(Vec::new());
        }

        self.conn_mut()
            .exec_map(
                "\
SELECT `mode`,
       `package_hash`,
       `package_format_version`,
       DATE_FORMAT(`applied_at`, '%Y-%m-%dT%H:%i:%s')
FROM `__squealy`.`publish_history`
ORDER BY `id` DESC
LIMIT ?",
                (limit as u64,),
                |(mode, package_hash, package_format_version, applied_at)| SchemaPublishRecord {
                    mode,
                    package_hash,
                    package_format_version,
                    applied_at,
                },
            )
            .await
            .map_err(MysqlError::Introspect)
    }

    async fn record_schema_publish(
        &mut self,
        mode: &str,
        package_hash: &str,
        package_format_version: &str,
    ) -> Result<(), MysqlError> {
        self.conn_mut()
            .query_drop("CREATE SCHEMA IF NOT EXISTS `__squealy`")
            .await
            .map_err(MysqlError::Execute)?;
        self.conn_mut()
            .query_drop(
                "\
CREATE TABLE IF NOT EXISTS `__squealy`.`publish_history` (
    `id` BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    `mode` VARCHAR(64) NOT NULL,
    `package_hash` VARCHAR(255) NOT NULL,
    `package_format_version` VARCHAR(64) NOT NULL,
    `applied_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
)",
            )
            .await
            .map_err(MysqlError::Execute)?;

        self.conn_mut()
            .exec_drop(
                "\
INSERT INTO `__squealy`.`publish_history` (
    `mode`,
    `package_hash`,
    `package_format_version`
)
VALUES (?, ?, ?)",
                (mode, package_hash, package_format_version),
            )
            .await
            .map_err(MysqlError::Execute)?;

        Ok(())
    }
}

/// Splits a rendered DDL script into individual statements. The renderer separates statements with
/// `;\n` and terminates the last with `;`. A comment or text default can itself contain `;\n`
/// inside a single-quoted literal, so the scan is quote-aware and only breaks outside a string —
/// otherwise a literal like `'a;\nb'` would be cut in two and, since MySQL auto-commits each
/// statement, fail the batch after earlier DDL had already committed.
fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            // The renderer escapes an embedded quote by doubling it (`''`); a simple toggle nets out
            // across the pair, correctly tracking both the quotes and the escape.
            b'\'' => in_string = !in_string,
            b';' if !in_string && bytes.get(index + 1) == Some(&b'\n') => {
                push_statement(&mut statements, &sql[start..index]);
                index += 1; // skip the '\n'
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    push_statement(&mut statements, &sql[start..]);
    statements
}

fn push_statement<'sql>(statements: &mut Vec<&'sql str>, statement: &'sql str) {
    let statement = statement.trim().trim_end_matches(';').trim();
    if !statement.is_empty() {
        statements.push(statement);
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_sql_type, split_statements};
    use squealy::SqlType;

    #[test]
    fn canonical_sql_type_maps_string_to_introspected_varchar() {
        // `String` renders as `VARCHAR(255)` and introspects back as `Varchar(255)`; canonicalizing the
        // desired side to that form is what keeps an incremental plan from churning forever.
        assert_eq!(canonical_sql_type(&SqlType::String), SqlType::Varchar(255));
        // Everything else is left untouched, including an explicitly-authored `Varchar`.
        assert_eq!(
            canonical_sql_type(&SqlType::Varchar(64)),
            SqlType::Varchar(64)
        );
        assert_eq!(canonical_sql_type(&SqlType::Text), SqlType::Text);
        assert_eq!(canonical_sql_type(&SqlType::I32), SqlType::I32);
    }

    #[test]
    fn canonical_sql_type_maps_uuid_to_char36() {
        // MySQL has no native `uuid`: a `uuid::Uuid` column renders as `CHAR(36)` and introspects back
        // as `Char(36)`, so the desired side must canonicalize to that or an incremental plan churns.
        assert_eq!(canonical_sql_type(&SqlType::Uuid), SqlType::Char(36));
    }

    #[test]
    fn split_statements_respects_string_literals() {
        // A `;\n` inside a single-quoted literal (e.g. a column comment) must not split the batch.
        let sql = "ALTER TABLE `t` MODIFY `c` INT COMMENT 'first;\nsecond';\n\
CREATE INDEX `i` ON `t` (`c`);";
        let statements = split_statements(sql);
        assert_eq!(
            statements,
            vec![
                "ALTER TABLE `t` MODIFY `c` INT COMMENT 'first;\nsecond'",
                "CREATE INDEX `i` ON `t` (`c`)",
            ]
        );
    }

    #[test]
    fn split_statements_handles_escaped_quotes() {
        let statements = split_statements("SET @x = 'a''b';\nSET @y = 1;");
        assert_eq!(statements, vec!["SET @x = 'a''b'", "SET @y = 1"]);
    }
}
