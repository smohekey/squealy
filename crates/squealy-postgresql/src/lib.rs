use std::fmt;

use squealy::{
    Backend, BindValue, Connection, ConnectionWithTransaction, Decode, InsertableTable,
    ProjectionShape, Returning, SelectBuilder, Table, TableProjection, UpdateableTable,
    build_delete, build_delete_returning, build_insert, build_insert_returning, build_select,
    build_update, build_update_returning,
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

trait PostgresQueryBuilder: query::PostgresExecutor {
    fn build_select_query<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> PostgresSelect<'_, Shape, Self>
    where
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        PostgresSelect::new(self, build_select::<Self, Shape>(f))
    }

    fn build_insert_query<S>(
        &self,
        columns: Vec<squealy::InsertColumn>,
    ) -> PostgresInsert<'_, S, (), Self>
    where
        S: InsertableTable,
    {
        PostgresInsert::new(self, build_insert::<S>(columns))
    }

    fn build_insert_returning_query<S, Shape>(
        &self,
        columns: Vec<squealy::InsertColumn>,
        returning: Vec<squealy::SelectColumn>,
    ) -> PostgresInsert<'_, S, Shape, Self>
    where
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        PostgresInsert::new(self, build_insert_returning::<S>(columns, returning))
    }

    fn build_update_query<S>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
    ) -> PostgresUpdate<'_, S, (), Self>
    where
        S: UpdateableTable,
    {
        PostgresUpdate::new(self, build_update::<S>(alias, columns, filters))
    }

    fn build_update_returning_query<S, Shape>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> PostgresUpdate<'_, S, Shape, Self>
    where
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        PostgresUpdate::new(
            self,
            build_update_returning::<S>(alias, columns, filters, returning),
        )
    }

    fn build_delete_query<S>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
    ) -> PostgresDelete<'_, S, (), Self>
    where
        S: TableProjection,
    {
        PostgresDelete::new(self, build_delete::<S>(alias, filters))
    }

    fn build_delete_returning_query<S, Shape>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> PostgresDelete<'_, S, Shape, Self>
    where
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        PostgresDelete::new(self, build_delete_returning::<S>(alias, filters, returning))
    }
}

impl<Conn> PostgresQueryBuilder for Conn where Conn: query::PostgresExecutor {}

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

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_select_query(f)
    }

    fn insert_query<S>(&self, columns: Vec<squealy::InsertColumn>) -> Self::Insert<'_, S, ()>
    where
        S: InsertableTable,
    {
        self.build_insert_query(columns)
    }

    fn insert_returning_query<S, Shape>(
        &self,
        columns: Vec<squealy::InsertColumn>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Insert<'_, S, Shape>
    where
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_insert_returning_query(columns, returning)
    }

    fn update_query<S>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
    ) -> Self::Update<'_, S, ()>
    where
        S: UpdateableTable,
    {
        self.build_update_query(alias, columns, filters)
    }

    fn update_returning_query<S, Shape>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Update<'_, S, Shape>
    where
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_update_returning_query(alias, columns, filters, returning)
    }

    fn delete_query<S>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
    ) -> Self::Delete<'_, S, ()>
    where
        S: TableProjection,
    {
        self.build_delete_query(alias, filters)
    }

    fn delete_returning_query<S, Shape>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Delete<'_, S, Shape>
    where
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_delete_returning_query(alias, filters, returning)
    }
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

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_select_query(f)
    }

    fn insert_query<S>(&self, columns: Vec<squealy::InsertColumn>) -> Self::Insert<'_, S, ()>
    where
        S: InsertableTable,
    {
        self.build_insert_query(columns)
    }

    fn insert_returning_query<S, Shape>(
        &self,
        columns: Vec<squealy::InsertColumn>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Insert<'_, S, Shape>
    where
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_insert_returning_query(columns, returning)
    }

    fn update_query<S>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
    ) -> Self::Update<'_, S, ()>
    where
        S: UpdateableTable,
    {
        self.build_update_query(alias, columns, filters)
    }

    fn update_returning_query<S, Shape>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Update<'_, S, Shape>
    where
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_update_returning_query(alias, columns, filters, returning)
    }

    fn delete_query<S>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
    ) -> Self::Delete<'_, S, ()>
    where
        S: TableProjection,
    {
        self.build_delete_query(alias, filters)
    }

    fn delete_returning_query<S, Shape>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Delete<'_, S, Shape>
    where
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_delete_returning_query(alias, filters, returning)
    }
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

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_select_query(f)
    }

    fn insert_query<S>(&self, columns: Vec<squealy::InsertColumn>) -> Self::Insert<'_, S, ()>
    where
        S: InsertableTable,
    {
        self.build_insert_query(columns)
    }

    fn insert_returning_query<S, Shape>(
        &self,
        columns: Vec<squealy::InsertColumn>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Insert<'_, S, Shape>
    where
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_insert_returning_query(columns, returning)
    }

    fn update_query<S>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
    ) -> Self::Update<'_, S, ()>
    where
        S: UpdateableTable,
    {
        self.build_update_query(alias, columns, filters)
    }

    fn update_returning_query<S, Shape>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Update<'_, S, Shape>
    where
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_update_returning_query(alias, columns, filters, returning)
    }

    fn delete_query<S>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
    ) -> Self::Delete<'_, S, ()>
    where
        S: TableProjection,
    {
        self.build_delete_query(alias, filters)
    }

    fn delete_returning_query<S, Shape>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Delete<'_, S, Shape>
    where
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        self.build_delete_returning_query(alias, filters, returning)
    }
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
