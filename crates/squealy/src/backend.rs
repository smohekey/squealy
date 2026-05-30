use std::io::{self, Write};

use crate::{
    DeleteBuilder, DeleteQuery, Filter, InsertColumn, InsertQuery, InsertableTable,
    ProjectionShape, Returning, SelectQuery, Table, TableProjection, UpdateColumn, UpdateQuery,
    UpdateableTable, build_delete_builder,
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

    type Insert<'conn, S, Shape>: InsertQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape;

    type Update<'conn, S, Shape>: UpdateQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape;

    type Delete<'conn, S, Shape>: DeleteQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut crate::SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape;

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
        Shape: ProjectionShape;

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
        Shape: ProjectionShape;

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
        Shape: ProjectionShape;
}
