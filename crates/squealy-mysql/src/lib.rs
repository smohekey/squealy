//! MySQL backend for squealy.
//!
//! Renders the MySQL dialect (backtick quoting, `AUTO_INCREMENT` identity, unsigned integers,
//! `VARCHAR`-backed strings) for both schema management (DDL/introspection against the core
//! `DatabaseModel`) and query execution. The query runtime lives in [`query`]; the single driver
//! `Conn` is held behind a [`tokio::sync::Mutex`] so the `&self` execution API can obtain the
//! `&mut Conn` that `mysql_async` requires.

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

pub use query::MysqlRowReader;

/// The MySQL schema backend marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mysql;

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
}

/// Maps a neutral [`SqlType`](squealy::SqlType) to the physical form the MySQL introspector reads
/// back, so a desired model does not churn against a live schema. MySQL has no key-usable unbounded
/// `text`, so bare `String` is rendered (and read back) as `VARCHAR(255)`.
fn canonical_sql_type(ty: &squealy::SqlType) -> squealy::SqlType {
    match ty {
        squealy::SqlType::String => squealy::SqlType::Varchar(255),
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
