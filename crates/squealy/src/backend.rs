use std::future::Future;
use std::io::{self, Write};

use crate::{
    DeleteBuilder, DeleteQuery, Filter, InsertColumn, InsertQuery, InsertableTable,
    ProjectionShape, Returning, SelectQuery, Table, TableProjection, UpdateColumn, UpdateQuery,
    UpdateableTable, build_delete_builder,
};

/// Backend-specific row cursor used while decoding a projected row.
pub trait RowReader: Sized {
    type Connection: Connection;

    fn read<T>(&mut self) -> Result<T, <Self::Connection as Connection>::Error>
    where
        T: Decode<Self::Connection>;
}

/// Decode a Rust value from a backend row reader.
pub trait Decode<Conn: Connection>: Sized {
    fn decode(row: &mut Conn::RowReader<'_>) -> Result<Self, Conn::Error>;
}

impl<Conn> Decode<Conn> for ()
where
    Conn: Connection,
{
    fn decode(_row: &mut Conn::RowReader<'_>) -> Result<Self, Conn::Error> {
        Ok(())
    }
}

/// Backend-specific DDL generation.
pub trait Backend: Sized {
    /// Generate backend-specific SQL for a table.
    fn write_table(&self, table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()>;
}

/// A backend connection that constructs select objects tied to that backend.
pub trait Connection: Sized {
    type Error;

    type RowReader<'row>: RowReader<Connection = Self>;

    fn no_rows_error() -> Self::Error;

    type Select<'conn, Shape>: SelectQuery<'conn, Connection = Self, Shape = Shape>
    where
        Self: 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    type Insert<'conn, S, Shape>: InsertQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    type Update<'conn, S, Shape>: UpdateQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    type Delete<'conn, S, Shape>: DeleteQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut crate::SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    fn insert<S>(&self) -> S::InsertBuilder<'_, Self>
    where
        S: InsertableTable,
    {
        S::insert_builder(self)
    }

    fn insert_query<S>(&self, columns: Vec<InsertColumn>) -> Self::Insert<'_, S, ()>
    where
        S: InsertableTable;

    fn insert_returning_query<S, Shape>(
        &self,
        columns: Vec<InsertColumn>,
        returning: Vec<crate::SelectColumn>,
    ) -> Self::Insert<'_, S, Shape>
    where
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    fn update<S>(&self) -> S::UpdateBuilder<'_, Self>
    where
        S: UpdateableTable,
    {
        S::update_builder(self)
    }

    fn update_query<S>(
        &self,
        alias: String,
        columns: Vec<UpdateColumn>,
        filters: Vec<Filter>,
    ) -> Self::Update<'_, S, ()>
    where
        S: UpdateableTable;

    fn update_returning_query<S, Shape>(
        &self,
        alias: String,
        columns: Vec<UpdateColumn>,
        filters: Vec<Filter>,
        returning: Vec<crate::SelectColumn>,
    ) -> Self::Update<'_, S, Shape>
    where
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;

    fn delete<'conn, S>(&'conn self) -> DeleteBuilder<'conn, 'static, Self, S>
    where
        S: TableProjection + 'conn,
    {
        build_delete_builder(self)
    }

    fn delete_query<S>(&self, alias: String, filters: Vec<Filter>) -> Self::Delete<'_, S, ()>
    where
        S: TableProjection;

    fn delete_returning_query<S, Shape>(
        &self,
        alias: String,
        filters: Vec<Filter>,
        returning: Vec<crate::SelectColumn>,
    ) -> Self::Delete<'_, S, Shape>
    where
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self>;
}

/// A connection that can run a closure inside a backend-managed transaction.
pub trait TransactionalConnection: Connection {
    type Transaction<'conn>: Connection<Error = Self::Error>
    where
        Self: 'conn;

    fn transaction<'conn, T, F>(
        &'conn mut self,
        f: F,
    ) -> impl Future<Output = Result<T, Self::Error>> + 'conn
    where
        T: 'conn,
        F: for<'tx> AsyncFnOnce(&'tx mut Self::Transaction<'conn>) -> Result<T, Self::Error>
            + 'conn;
}
