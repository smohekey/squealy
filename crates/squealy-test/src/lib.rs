#![forbid(unsafe_code)]

use squealy::{
    Backend, Connection, ConnectionWithTransaction, Decode, InsertableTable, Projectable,
    ProjectionShape, QueryBuilder, SelectAst, SupportsReturning, Table, TableProjection,
    UpdateableTable,
};

mod query;
mod sql;

pub use query::{
    EmptyRows, TestDelete, TestInsert, TestParam, TestParamWriter, TestPreparedMutation,
    TestPreparedSelect, TestRowReader, TestSelect, TestUpdate,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestConnection;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestBackend;

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
    use crate::ConnectionWithTransaction;

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
        let rows = vec![1, 2, 3, 4];
        let mut conn = TestConnection;
        let sum = block_on(conn.transaction_scoped(move |_tx| {
            Box::pin(async move { Ok::<i32, TestError>(rows.iter().sum()) })
        }))
        .expect("transaction");
        assert_eq!(sum, 10);
    }
}
