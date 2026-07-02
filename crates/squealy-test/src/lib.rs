#![forbid(unsafe_code)]

use squealy::{
    Backend, Connection, ConnectionWithTransaction, Decode, InsertableTable, Projectable,
    ProjectionShape, QueryBuilder, SelectAst, SupportsReturning, Table, TableProjection,
    UpdateableTable,
};

mod query;
mod sql;

pub use query::{
    EmptyRows, TestDelete, TestDeleteUsing, TestInsert, TestParam, TestParamWriter,
    TestPreparedMutation, TestPreparedSelect, TestRowReader, TestSelect, TestUpdate,
    TestUpdateFrom,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestConnection;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestBackend;

// The test backend renders `FOR UPDATE` / `FOR SHARE`, so the row-lock builders are available.
impl squealy::RendersRowLock for TestBackend {}

// The test backend mirrors Postgres/MySQL, which support `INTERSECT ALL` / `EXCEPT ALL`.
impl squealy::SupportsIntersectExceptAll for TestBackend {}

// The test backend mirrors Postgres/MySQL, which can render a columnless (all-default-row) upsert.
impl squealy::SupportsColumnlessUpsert for TestBackend {}

// The test backend mirrors Postgres/MySQL, which accept the `DEFAULT` keyword as an assignment value.
impl squealy::SupportsDefaultKeyword for TestBackend {}

// The test backend mirrors Postgres/MySQL, which support `EXTRACT(<field> FROM <ts>)`.
impl squealy::SupportsExtract for TestBackend {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestError {
    NoRows,
}

impl Backend for TestBackend {
    type Error = TestError;

    type RowReader<'row> = TestRowReader<'row>;

    type ParamWriter<'param> = query::TestParamWriter<'param>;

    type Param = query::TestParam;

    fn param_writer(params: &mut Vec<Self::Param>) -> Self::ParamWriter<'_> {
        query::TestParamWriter::new(params)
    }

    fn no_rows_error() -> Self::Error {
        TestError::NoRows
    }

    fn write_table(
        &self,
        table: &(dyn Table + Sync),
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::write_table(table, writer)
    }
}

// The test backend renders a generic dialect and exercises the returning builders.
impl SupportsReturning for TestBackend {}
// Deliberately does NOT implement `SupportsFullJoin` (it stands in for a MySQL-like dialect with no
// FULL JOIN), so the compile-fail suite can assert `full_join` is gated. `right_join` is ungated and
// works here; full-join *rendering* is covered by the PostgreSQL backend tests.

impl QueryBuilder for TestConnection {
    type Backend = TestBackend;

    type Select<'conn, 'scope, Base, Shape, Projection>
        = TestSelect<'conn, 'scope, Shape, Base, Projection>
    where
        Self: 'conn,
        Base: SelectAst<'conn, 'scope, Self> + 'conn,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Projection: Projectable;

    type Insert<'conn, S, Shape, Rows, Returning>
        = TestInsert<'conn, S, Shape, Rows, Returning>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Rows: squealy::InsertRows,
        Returning: Projectable;

    type Update<'conn, S, Shape, Columns, Filters, Returning>
        = TestUpdate<'conn, S, Shape, Columns, Filters, Returning>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Columns: squealy::UpdateAssignments,
        Filters: squealy::PredicateNodes,
        Returning: Projectable;

    type Delete<'conn, S, Shape, Filters, Returning>
        = TestDelete<'conn, S, Shape, Filters, Returning>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Filters: squealy::PredicateNodes,
        Returning: Projectable;

    type UpdateFrom<'conn, S, O, Columns, Filters>
        = TestUpdateFrom<'conn, S, O, Columns, Filters>
    where
        Self: 'conn,
        S: UpdateableTable,
        O: squealy::SchemaTable,
        Columns: squealy::UpdateAssignments,
        Filters: squealy::PredicateNodes;

    type DeleteUsing<'conn, S, O, Filters>
        = TestDeleteUsing<'conn, S, O, Filters>
    where
        Self: 'conn,
        S: TableProjection,
        O: TableProjection,
        Filters: squealy::PredicateNodes;
}

impl Connection for TestConnection {}

impl ConnectionWithTransaction for TestConnection {
    type Transaction<'conn>
        = TestConnection
    where
        Self: 'conn;

    async fn transaction<'conn, T, F>(
        &'conn mut self,
        f: F,
    ) -> Result<T, <Self::Backend as Backend>::Error>
    where
        T: 'conn,
        F: for<'tx> AsyncFnOnce(
                &'tx mut Self::Transaction<'conn>,
            ) -> Result<T, <Self::Backend as Backend>::Error>
            + 'conn,
    {
        let mut transaction = TestConnection;
        f(&mut transaction).await
    }

    async fn transaction_scoped<'conn, T, F>(
        &'conn mut self,
        f: F,
    ) -> Result<T, <Self::Backend as Backend>::Error>
    where
        T: 'conn,
        F: for<'tx> FnOnce(
                &'tx mut Self::Transaction<'conn>,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<T, <Self::Backend as Backend>::Error>>
                        + Send
                        + 'tx,
                >,
            > + 'conn,
    {
        let mut transaction = TestConnection;
        f(&mut transaction).await
    }
}

#[cfg(test)]
mod transaction_scoped_tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::{TestConnection, TestError};
    use crate::{Backend, ConnectionWithTransaction};

    /// Compile-time guard: the future returned by `transaction_scoped` is `Send` even when the
    /// connection is known only generically (`C: ConnectionWithTransaction`) — the backend-agnostic
    /// multithreaded `-> impl Future + Send` service workflow. Mirrors the `async_trait_send`
    /// regression test, but for the transaction entry point. Never executed; it only needs to
    /// type-check.
    #[allow(dead_code)]
    fn outer_future_is_send_for_generic_backend<C>(conn: &mut C)
    where
        C: ConnectionWithTransaction,
        <C::Backend as Backend>::Error: Send,
    {
        fn assert_send<T: Send>(_: T) {}
        assert_send(conn.transaction_scoped(move |_tx| {
            Box::pin(async move { Ok::<(), <C::Backend as Backend>::Error>(()) })
        }));
    }

    /// Minimal executor (no tokio dep): the test futures never suspend, so busy-polling
    /// to completion is enough.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    /// Proves a closure that *owns* captured data (moved into the future) satisfies
    /// `transaction_scoped`'s HRTB bound — the case an `async` closure (`transaction`)
    /// cannot express. Only `tx` is borrowed; the data is owned, so the future is valid for
    /// any transaction lifetime.
    #[test]
    fn admits_captured_data() {
        let rows: Vec<i32> = (1..=4).collect();
        let mut conn = TestConnection;
        let sum = block_on(conn.transaction_scoped(move |_tx| {
            Box::pin(async move { Ok::<i32, TestError>(rows.iter().sum()) })
        }))
        .expect("transaction");
        assert_eq!(sum, 10);
    }
}
