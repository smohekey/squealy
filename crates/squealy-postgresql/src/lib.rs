use std::fmt;

use squealy::{
    Backend, BindValue, Connection, Decode, InsertableTable, ProjectionShape, Returning,
    SelectBuilder, Table, TableProjection, UpdateableTable, build_delete, build_delete_returning,
    build_insert, build_insert_returning, build_select, build_update, build_update_returning,
};
use tokio_postgres::Client;

mod query;
mod sql;

pub use query::{
    EmptyRows, PostgresDelete, PostgresInsert, PostgresRowReader, PostgresSelect, PostgresUpdate,
};

pub struct PostgresConnection {
    client: Option<Client>,
}

impl PostgresConnection {
    pub fn new(client: Client) -> Self {
        Self {
            client: Some(client),
        }
    }

    pub const fn no_driver() -> Self {
        Self { client: None }
    }

    pub(crate) fn client(&self) -> Result<&Client, PostgresError> {
        self.client.as_ref().ok_or(PostgresError::NoDriver)
    }
}

impl Default for PostgresConnection {
    fn default() -> Self {
        Self::no_driver()
    }
}

impl fmt::Debug for PostgresConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresConnection")
            .field("has_client", &self.client.is_some())
            .finish()
    }
}

#[allow(non_upper_case_globals)]
pub const PostgresConnection: PostgresConnection = PostgresConnection::no_driver();

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

impl Backend for PostgresConnection {
    fn write_table(
        &self,
        table: &(dyn Table + Sync),
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::write_table(table, writer)
    }
}

impl Connection for PostgresConnection {
    type Error = PostgresError;

    type RowReader<'row>
        = PostgresRowReader<'row>
    where
        Self: 'row;

    fn no_rows_error() -> Self::Error {
        PostgresError::NoRows
    }

    type Select<'conn, Shape>
        = PostgresSelect<'conn, Shape>
    where
        Self: 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    type Insert<'conn, S, Shape>
        = PostgresInsert<'conn, S, Shape>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    type Update<'conn, S, Shape>
        = PostgresUpdate<'conn, S, Shape>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    type Delete<'conn, S, Shape>
        = PostgresDelete<'conn, S, Shape>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>,
    {
        PostgresSelect::new(self, build_select::<Self, Shape>(f))
    }

    fn insert_query<S>(&self, columns: Vec<squealy::InsertColumn>) -> Self::Insert<'_, S, ()>
    where
        S: InsertableTable,
    {
        PostgresInsert::new(self, build_insert::<S>(columns))
    }

    fn insert_returning_query<S, Shape>(
        &self,
        columns: Vec<squealy::InsertColumn>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Insert<'_, S, Shape>
    where
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>,
    {
        PostgresInsert::new(self, build_insert_returning::<S>(columns, returning))
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
        PostgresUpdate::new(self, build_update::<S>(alias, columns, filters))
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
        Shape::Row: Decode<Self>,
    {
        PostgresUpdate::new(
            self,
            build_update_returning::<S>(alias, columns, filters, returning),
        )
    }

    fn delete_query<S>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
    ) -> Self::Delete<'_, S, ()>
    where
        S: TableProjection,
    {
        PostgresDelete::new(self, build_delete::<S>(alias, filters))
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
        Shape::Row: Decode<Self>,
    {
        PostgresDelete::new(self, build_delete_returning::<S>(alias, filters, returning))
    }
}
