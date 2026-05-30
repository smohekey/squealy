use std::fmt;

use squealy::{
    Backend, BindValue, Connection, ConnectionWithTransaction, Decode, InsertableTable,
    ProjectionShape, Table, TableProjection, UpdateableTable,
};
use tokio_postgres::Client;

mod query;
mod sql;

pub use query::{
    EmptyRows, PostgresDelete, PostgresInsert, PostgresRowReader, PostgresSelect, PostgresUpdate,
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
    NoDriver,
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

impl Connection for Postgres {
    type Backend = Postgres;

    type Select<'conn, Shape>
        = PostgresSelect<'conn, Shape, Self>
    where
        Self: 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Insert<'conn, S, Shape>
        = PostgresInsert<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Update<'conn, S, Shape>
        = PostgresUpdate<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Delete<'conn, S, Shape>
        = PostgresDelete<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;
}

impl Connection for PostgresConnection {
    type Backend = Postgres;

    type Select<'conn, Shape>
        = PostgresSelect<'conn, Shape, Self>
    where
        Self: 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Insert<'conn, S, Shape>
        = PostgresInsert<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Update<'conn, S, Shape>
        = PostgresUpdate<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Delete<'conn, S, Shape>
        = PostgresDelete<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;
}

impl Connection for PostgresTransaction<'_> {
    type Backend = Postgres;

    type Select<'conn, Shape>
        = PostgresSelect<'conn, Shape, Self>
    where
        Self: 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Insert<'conn, S, Shape>
        = PostgresInsert<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Update<'conn, S, Shape>
        = PostgresUpdate<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Delete<'conn, S, Shape>
        = PostgresDelete<'conn, S, Shape, Self>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;
}

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
