//! MySQL schema-management backend for squealy.
//!
//! This crate is deliberately **schema-only** (no query backend): it implements the DDL-management
//! traits against the core `DatabaseModel`. Its purpose is partly to keep the crate boundaries
//! honest — a second backend that renders a different dialect (backtick quoting, `AUTO_INCREMENT`,
//! unsigned integers, `VARCHAR`-backed strings) without touching core or the model.

#![forbid(unsafe_code)]

use std::fmt;
use std::io::Write;

use mysql_async::prelude::Queryable;
use squealy::{
    DatabaseModel, DdlExecutor, SchemaBackend, SchemaConnect, SchemaIntrospect, SchemaRefactorStore,
};

mod introspect;
mod sql;

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

/// A live MySQL connection used for schema management.
pub struct MysqlConnection {
    conn: mysql_async::Conn,
}

impl MysqlConnection {
    pub fn new(conn: mysql_async::Conn) -> Self {
        Self { conn }
    }
}

impl fmt::Debug for MysqlConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MysqlConnection").finish()
    }
}

/// An error connecting to or executing DDL against MySQL.
#[derive(Debug)]
pub enum MysqlError {
    Connect(mysql_async::Error),
    Execute(mysql_async::Error),
    Introspect(mysql_async::Error),
}

impl fmt::Display for MysqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MysqlError::Connect(error) => write!(formatter, "mysql connect error: {error}"),
            MysqlError::Execute(error) => write!(formatter, "mysql ddl error: {error}"),
            MysqlError::Introspect(error) => {
                write!(formatter, "mysql introspection error: {error}")
            }
        }
    }
}

impl std::error::Error for MysqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MysqlError::Connect(error)
            | MysqlError::Execute(error)
            | MysqlError::Introspect(error) => Some(error),
        }
    }
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
    /// PostgreSQL backend this is *not* atomic: a mid-batch failure leaves earlier statements applied.
    /// (A real boundary difference the `DdlExecutor` contract anticipates.)
    async fn execute_ddl(&mut self, sql: &str) -> Result<(), MysqlError> {
        for statement in split_statements(sql) {
            self.conn
                .query_drop(statement)
                .await
                .map_err(MysqlError::Execute)?;
        }
        Ok(())
    }
}

impl SchemaIntrospect for MysqlConnection {
    type Error = MysqlError;

    async fn introspect_database(&mut self) -> Result<DatabaseModel, MysqlError> {
        introspect::database(&mut self.conn).await
    }
}

impl SchemaRefactorStore for MysqlConnection {
    type Error = MysqlError;

    async fn applied_refactor_ids(&mut self) -> Result<Vec<String>, MysqlError> {
        let exists = self
            .conn
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

        self.conn
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

        self.conn
            .query_drop("CREATE SCHEMA IF NOT EXISTS `__squealy`")
            .await
            .map_err(MysqlError::Execute)?;
        self.conn
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
            self.conn
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

/// Splits a rendered DDL script into individual statements. The renderer separates statements with
/// `;\n` and terminates the last with `;`, and never emits `;\n` inside a statement.
fn split_statements(sql: &str) -> impl Iterator<Item = &str> {
    sql.split(";\n")
        .map(|statement| statement.trim().trim_end_matches(';').trim())
        .filter(|statement| !statement.is_empty())
}
