use std::future::Future;

use crate::{
    Backend, Decode, DeleteBuilder, DeleteQuery, InsertQuery, InsertableTable, ProjectionShape,
    Returning, SelectQuery, TableProjection, UpdateQuery, UpdateableTable, build_delete_builder,
    build_select,
};

/// A backend connection that constructs query objects tied to that backend.
pub trait Connection: Sized {
    type Backend: Backend;

    type Select<'conn, Shape>: SelectQuery<'conn, Connection = Self, Shape = Shape>
    where
        Self: 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Insert<'conn, S, Shape>: InsertQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Update<'conn, S, Shape>: UpdateQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    type Delete<'conn, S, Shape>: DeleteQuery<'conn, Connection = Self, Table = S, Shape = Shape>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut crate::SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
    {
        <Self::Select<'_, Shape> as SelectQuery<'_>>::build(self, build_select::<Self, Shape>(f))
    }

    fn insert<S>(&self) -> S::InsertBuilder<'_, Self>
    where
        S: InsertableTable,
    {
        S::insert_builder(self)
    }

    fn update<S>(&self) -> S::UpdateBuilder<'_, Self>
    where
        S: UpdateableTable,
    {
        S::update_builder(self)
    }

    fn delete<'conn, S>(&'conn self) -> DeleteBuilder<'conn, 'static, Self, S>
    where
        S: TableProjection + 'conn,
    {
        build_delete_builder(self)
    }
}

/// A root backend connection that can run a closure inside a backend-managed transaction.
pub trait ConnectionWithTransaction: Connection {
    type Transaction<'conn>: Connection<Backend = Self::Backend>
    where
        Self: 'conn;

    fn transaction<'conn, T, F>(
        &'conn mut self,
        f: F,
    ) -> impl Future<Output = Result<T, <Self::Backend as Backend>::Error>> + 'conn
    where
        T: 'conn,
        F: for<'tx> AsyncFnOnce(
                &'tx mut Self::Transaction<'conn>,
            ) -> Result<T, <Self::Backend as Backend>::Error>
            + 'conn;
}
