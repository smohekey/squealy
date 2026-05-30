use squealy::{
    Backend, Connection, InsertableTable, ProjectionShape, Returning, SelectBuilder, Table,
    TableProjection, UpdateableTable, build_delete, build_delete_returning, build_insert,
    build_insert_returning, build_select, build_update, build_update_returning,
};

mod query;
mod sql;

pub use query::{EmptyRows, PostgresDelete, PostgresInsert, PostgresSelect, PostgresUpdate};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PostgresConnection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostgresError {
    NoDriver,
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

    type Select<'conn, Shape>
        = PostgresSelect<'conn, Shape>
    where
        Self: 'conn,
        Shape: ProjectionShape;

    type Insert<'conn, S, Shape>
        = PostgresInsert<'conn, S, Shape>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape;

    type Update<'conn, S, Shape>
        = PostgresUpdate<'conn, S, Shape>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape;

    type Delete<'conn, S, Shape>
        = PostgresDelete<'conn, S, Shape>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
    {
        PostgresSelect::new(build_select::<Self, Shape>(f))
    }

    fn insert_query<S>(&self, columns: Vec<squealy::InsertColumn>) -> Self::Insert<'_, S, ()>
    where
        S: InsertableTable,
    {
        PostgresInsert::new(build_insert::<S>(columns))
    }

    fn insert_returning_query<S, Shape>(
        &self,
        columns: Vec<squealy::InsertColumn>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Insert<'_, S, Shape>
    where
        S: InsertableTable,
        Shape: ProjectionShape,
    {
        PostgresInsert::new(build_insert_returning::<S>(columns, returning))
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
        PostgresUpdate::new(build_update::<S>(alias, columns, filters))
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
    {
        PostgresUpdate::new(build_update_returning::<S>(
            alias, columns, filters, returning,
        ))
    }

    fn delete_query<S>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
    ) -> Self::Delete<'_, S, ()>
    where
        S: TableProjection,
    {
        PostgresDelete::new(build_delete::<S>(alias, filters))
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
    {
        PostgresDelete::new(build_delete_returning::<S>(alias, filters, returning))
    }
}
