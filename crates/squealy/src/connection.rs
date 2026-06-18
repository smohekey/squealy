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

    /// Like [`transaction`](Self::transaction), but the closure returns a **boxed** future,
    /// so it can carry per-call data into the transaction.
    ///
    /// An `async` closure that captures data cannot satisfy the higher-ranked `AsyncFnOnce`
    /// bound of [`transaction`](Self::transaction) — a Rust async-closure limitation. A plain
    /// `FnOnce` returning `Pin<Box<dyn Future + 'tx>>` boxes the per-call future instead. The
    /// data must be **moved into** the future (owned) so that only `tx` is borrowed for `'tx`
    /// — borrowing outer data would re-introduce the higher-ranked conflict. The boxed future
    /// is required to be `Send` and the returned future is itself `Send`, so this stays usable
    /// from a multithreaded `-> impl Future + Send` service — including a *backend-generic* one
    /// (`C: ConnectionWithTransaction`), not just a concrete connection (see the
    /// `async_trait_send` regression test and `outer_future_is_send_for_generic_backend`).
    /// Callers write:
    ///
    /// ```ignore
    /// conn.transaction_scoped(move |tx| Box::pin(async move {
    ///     // `rows` moved in (owned); `tx` borrowed
    ///     for row in rows { tx.to::<T>()./* … */.insert().await?; }
    ///     Ok(())
    /// })).await
    /// ```
    fn transaction_scoped<'conn, T, F>(
        &'conn mut self,
        f: F,
    ) -> impl Future<Output = Result<T, <Self::Backend as Backend>::Error>> + Send + 'conn
    where
        T: Send + 'conn,
        F: for<'tx> FnOnce(
                &'tx mut Self::Transaction<'conn>,
            ) -> std::pin::Pin<
                Box<
                    dyn Future<Output = Result<T, <Self::Backend as Backend>::Error>> + Send + 'tx,
                >,
            > + Send
            + 'conn;
}
