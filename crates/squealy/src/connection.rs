use std::future::Future;

use crate::{
    Backend, Decode, DeleteQuery, From, HCons, HNil, InsertQuery, InsertableTable, Projectable,
    ProjectionShape, ReturningProjection, RootSource, SelectAst, SelectQuery, TableProjection,
    ToColumns, UpdateQuery, UpdateableTable, WriteableTable,
};

/// A backend-specific handle that constructs query objects.
pub trait QueryBuilder: Sized {
    type Backend: Backend;

    type Select<'builder, 'scope, Base, Shape, Projection>: SelectQuery<'builder, 'scope, Base, Projection, Builder = Self, Shape = Shape>
    where
        Self: 'builder,
        Base: 'builder,
        Base: SelectAst<'builder, 'scope, Self>,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Projection: Projectable;

    type Insert<'builder, S, Shape, Rows, Returning>: InsertQuery<'builder, Rows, Returning, Builder = Self, Table = S, Shape = Shape>
    where
        Self: 'builder,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Rows: crate::InsertRows,
        Returning: Projectable;

    type Update<'builder, S, Shape, Columns, Filters, Returning>: UpdateQuery<'builder, Columns, Filters, Returning, Builder = Self, Table = S, Shape = Shape>
    where
        Self: 'builder,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Columns: crate::UpdateAssignments,
        Filters: crate::PredicateNodes,
        Returning: Projectable;

    type Delete<'builder, S, Shape, Filters, Returning>: DeleteQuery<'builder, Filters, Returning, Builder = Self, Table = S, Shape = Shape>
    where
        Self: 'builder,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Filters: crate::PredicateNodes,
        Returning: Projectable;

    fn select<P>(
        &self,
        projection: P,
    ) -> Self::Select<
        '_,
        'static,
        crate::NoSources<'_, Self>,
        <P as ReturningProjection<'static>>::Shape,
        P,
    >
    where
        P: ReturningProjection<'static> + Projectable,
        <P as ReturningProjection<'static>>::Shape: ProjectionShape,
        <<P as ReturningProjection<'static>>::Shape as ProjectionShape>::Row: Decode<Self::Backend>,
    {
        crate::query::build_sourceless_select(self, projection)
    }

    fn from<S>(
        &self,
    ) -> From<'_, '_, Self, HCons<<S as ProjectionShape>::Exprs<'_>, HNil>, RootSource<S>>
    where
        S: TableProjection,
    {
        crate::query::build_from_builder(self)
    }

    fn to<S>(&self) -> S::WriteBuilder<'_, Self>
    where
        S: WriteableTable,
    {
        S::write_builder(self)
    }

    fn to_columns<S, Columns>(&self) -> ToColumns<'_, Self, S, Columns>
    where
        S: TableProjection,
    {
        ToColumns::new(self)
    }
}

/// A backend query builder that can execute the query objects it constructs.
pub trait Connection: QueryBuilder {}

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
