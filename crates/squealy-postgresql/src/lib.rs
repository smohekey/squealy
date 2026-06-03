#![forbid(unsafe_code)]

use std::fmt;

use squealy::{
    Backend, BindValue, Connection, ConnectionWithTransaction, Decode, InsertableTable,
    Projectable, ProjectionShape, QueryBuilder, SelectAst, Table, TableProjection, UpdateableTable,
};
use tokio_postgres::Client;

#[cfg(feature = "schema")]
mod introspect;
mod query;
mod sql;

pub use query::{
    EmptyRows, PostgresDelete, PostgresInsert, PostgresPreparedMutation, PostgresPreparedSelect,
    PostgresRowReader, PostgresSelect, PostgresUpdate,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Postgres;

pub struct PostgresConnection {
    client: Client,
}

impl PostgresConnection {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }
}

pub struct PostgresTransaction<'conn> {
    pub(crate) transaction: tokio_postgres::Transaction<'conn>,
}

impl fmt::Debug for PostgresTransaction<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PostgresTransaction").finish()
    }
}

impl fmt::Debug for PostgresConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PostgresConnection").finish()
    }
}

#[derive(Debug)]
pub enum PostgresError {
    NoRows,
    UnsupportedBind(BindValue),
    Database(tokio_postgres::Error),
    Decode(tokio_postgres::Error),
    Conversion(&'static str),
}

impl From<tokio_postgres::Error> for PostgresError {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::Database(error)
    }
}

impl fmt::Display for PostgresError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PostgresError::NoRows => formatter.write_str("query returned no rows"),
            PostgresError::UnsupportedBind(value) => {
                write!(formatter, "unsupported bind value: {value:?}")
            }
            PostgresError::Database(error) => write!(formatter, "database error: {error}"),
            PostgresError::Decode(error) => write!(formatter, "decode error: {error}"),
            PostgresError::Conversion(target) => {
                write!(formatter, "could not convert value to {target}")
            }
        }
    }
}

impl std::error::Error for PostgresError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PostgresError::Database(error) | PostgresError::Decode(error) => Some(error),
            _ => None,
        }
    }
}

impl Backend for Postgres {
    type Error = PostgresError;

    type RowReader<'row> = PostgresRowReader<'row>;

    fn no_rows_error() -> Self::Error {
        PostgresError::NoRows
    }

    fn write_table(
        &self,
        table: &(dyn Table + Sync),
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::write_table(table, writer)
    }
}

#[cfg(feature = "schema")]
impl squealy::SchemaBackend for Postgres {
    fn capabilities(&self) -> squealy::SchemaCapabilities {
        squealy::SchemaCapabilities {
            constraints: squealy::ConstraintCapabilities {
                foreign_key_match_type: true,
                foreign_key_deferrability: true,
                foreign_key_validation: true,
                foreign_key_enforcement: false,
                check_validation: true,
                check_enforcement: false,
            },
            indexes: squealy::IndexCapabilities {
                predicates: true,
                expressions: true,
                include_columns: true,
                null_ordering: true,
                collations: true,
                operator_classes: true,
            },
        }
    }

    fn render_create(
        &self,
        model: &squealy::DatabaseModel,
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::ddl::write_database(model, writer)
    }

    fn render_plan(
        &self,
        plan: &squealy::DatabasePlan,
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::ddl::write_plan(plan, writer)
    }
}

#[cfg(feature = "schema")]
impl squealy::DdlExecutor for PostgresConnection {
    type Error = PostgresError;

    /// Runs the DDL batch inside a transaction so create-from-scratch is all-or-nothing
    /// (PostgreSQL supports transactional DDL).
    async fn execute_ddl(&mut self, sql: &str) -> Result<(), PostgresError> {
        let transaction = self.client_mut().transaction().await?;
        transaction.batch_execute(sql).await?;
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(feature = "schema")]
impl squealy::SchemaConnect for Postgres {
    type Connection = PostgresConnection;
    type Error = PostgresError;

    async fn connect(&self, url: &str) -> Result<PostgresConnection, PostgresError> {
        let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
        // Drive the connection's IO task in the background for the life of the client.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(PostgresConnection::new(client))
    }
}

#[cfg(feature = "schema")]
impl squealy::SchemaIntrospect for PostgresConnection {
    type Error = PostgresError;

    async fn introspect_database(&mut self) -> Result<squealy::DatabaseModel, PostgresError> {
        introspect::database(self.client()).await
    }
}

#[cfg(feature = "schema")]
impl squealy::SchemaRefactorStore for PostgresConnection {
    type Error = PostgresError;

    async fn applied_refactor_ids(&mut self) -> Result<Vec<String>, PostgresError> {
        let exists = self
            .client()
            .query_one(
                "SELECT to_regclass('\"__squealy\".\"refactors\"') IS NOT NULL",
                &[],
            )
            .await?
            .get::<_, bool>(0);
        if !exists {
            return Ok(Vec::new());
        }

        Ok(self
            .client()
            .query(
                "SELECT \"id\" FROM \"__squealy\".\"refactors\" ORDER BY \"id\"",
                &[],
            )
            .await?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }

    async fn record_applied_refactor_ids(&mut self, ids: &[String]) -> Result<(), PostgresError> {
        if ids.is_empty() {
            return Ok(());
        }

        self.client()
            .batch_execute(
                "\
CREATE SCHEMA IF NOT EXISTS \"__squealy\";
CREATE TABLE IF NOT EXISTS \"__squealy\".\"refactors\" (
    \"id\" text PRIMARY KEY,
    \"applied_at\" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
)",
            )
            .await?;

        for id in ids {
            self.client()
                .execute(
                    "\
INSERT INTO \"__squealy\".\"refactors\" (\"id\")
VALUES ($1)
ON CONFLICT (\"id\") DO NOTHING",
                    &[id],
                )
                .await?;
        }

        Ok(())
    }
}

impl QueryBuilder for Postgres {
    type Backend = Postgres;

    type Select<'conn, 'scope, Base, Shape, Projection>
        = PostgresSelect<'conn, 'scope, Shape, Base, Projection, Self>
    where
        Self: 'conn,
        Base: SelectAst<'conn, 'scope, Self> + 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Projection: Projectable;

    type Insert<'conn, S, Shape, Rows, Returning>
        = PostgresInsert<'conn, S, Shape, Rows, Returning, Self>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Rows: squealy::InsertRows,
        Returning: Projectable;

    type Update<'conn, S, Shape, Columns, Filters, Returning>
        = PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Self>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Columns: squealy::UpdateAssignments,
        Filters: squealy::PredicateNodes,
        Returning: Projectable;

    type Delete<'conn, S, Shape, Filters, Returning>
        = PostgresDelete<'conn, S, Shape, Filters, Returning, Self>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Filters: squealy::PredicateNodes,
        Returning: Projectable;
}

impl QueryBuilder for PostgresConnection {
    type Backend = Postgres;

    type Select<'conn, 'scope, Base, Shape, Projection>
        = PostgresSelect<'conn, 'scope, Shape, Base, Projection, Self>
    where
        Self: 'conn,
        Base: SelectAst<'conn, 'scope, Self> + 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Projection: Projectable;

    type Insert<'conn, S, Shape, Rows, Returning>
        = PostgresInsert<'conn, S, Shape, Rows, Returning, Self>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Rows: squealy::InsertRows,
        Returning: Projectable;

    type Update<'conn, S, Shape, Columns, Filters, Returning>
        = PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Self>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Columns: squealy::UpdateAssignments,
        Filters: squealy::PredicateNodes,
        Returning: Projectable;

    type Delete<'conn, S, Shape, Filters, Returning>
        = PostgresDelete<'conn, S, Shape, Filters, Returning, Self>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Filters: squealy::PredicateNodes,
        Returning: Projectable;
}

impl QueryBuilder for PostgresTransaction<'_> {
    type Backend = Postgres;

    type Select<'conn, 'scope, Base, Shape, Projection>
        = PostgresSelect<'conn, 'scope, Shape, Base, Projection, Self>
    where
        Self: 'conn,
        Base: SelectAst<'conn, 'scope, Self> + 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Projection: Projectable;

    type Insert<'conn, S, Shape, Rows, Returning>
        = PostgresInsert<'conn, S, Shape, Rows, Returning, Self>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Rows: squealy::InsertRows,
        Returning: Projectable;

    type Update<'conn, S, Shape, Columns, Filters, Returning>
        = PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Self>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Columns: squealy::UpdateAssignments,
        Filters: squealy::PredicateNodes,
        Returning: Projectable;

    type Delete<'conn, S, Shape, Filters, Returning>
        = PostgresDelete<'conn, S, Shape, Filters, Returning, Self>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Filters: squealy::PredicateNodes,
        Returning: Projectable;
}

impl Connection for PostgresConnection {}

impl Connection for PostgresTransaction<'_> {}

impl ConnectionWithTransaction for PostgresConnection {
    type Transaction<'conn>
        = PostgresTransaction<'conn>
    where
        Self: 'conn;

    fn transaction<'conn, T, F>(
        &'conn mut self,
        f: F,
    ) -> impl std::future::Future<Output = Result<T, <Self::Backend as Backend>::Error>> + 'conn
    where
        T: 'conn,
        F: for<'tx> AsyncFnOnce(
                &'tx mut Self::Transaction<'conn>,
            ) -> Result<T, <Self::Backend as Backend>::Error>
            + 'conn,
    {
        async move {
            let transaction = self
                .client_mut()
                .transaction()
                .await
                .map_err(PostgresError::Database)?;
            let mut transaction: Self::Transaction<'conn> = PostgresTransaction { transaction };

            match f(&mut transaction).await {
                Ok(value) => {
                    transaction
                        .transaction
                        .commit()
                        .await
                        .map_err(PostgresError::Database)?;
                    Ok(value)
                }
                Err(error) => {
                    transaction
                        .transaction
                        .rollback()
                        .await
                        .map_err(PostgresError::Database)?;
                    Err(error)
                }
            }
        }
    }
}
