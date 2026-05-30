use std::io::{self, Write};

use crate::{
    DeleteBuilder, DeleteQuery, InsertQuery, InsertableTable, ProjectionShape, Returning,
    SelectQuery, Table, TableProjection,
};

/// Backend-specific DDL generation.
pub trait Backend: Sized {
    /// Generate backend-specific SQL for a table.
    fn write_table(&self, table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()>;
}

/// A backend connection that constructs select objects tied to that backend.
pub trait Connection: Sized {
    type Error;

    type Select<'conn, Shape>: SelectQuery<'conn, Connection = Self, Shape = Shape>
    where
        Self: 'conn,
        Shape: ProjectionShape;

    type Insert<'conn, S>: InsertQuery<'conn, Connection = Self, Table = S>
    where
        Self: 'conn,
        S: InsertableTable;

    type Delete<'conn, S>: DeleteQuery<'conn, Connection = Self, Table = S>
    where
        Self: 'conn,
        S: TableProjection;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut crate::SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape;

    fn insert<S>(&self, row: S::WithColumn<'static, crate::ColumnValue>) -> Self::Insert<'_, S>
    where
        S: InsertableTable;

    fn delete<S>(
        &self,
        f: impl for<'scope> FnOnce(&mut DeleteBuilder<'_, 'scope, Self, S>),
    ) -> Self::Delete<'_, S>
    where
        S: TableProjection;
}
