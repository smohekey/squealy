use squealy::{
    Backend, Connection, ConnectionWithTransaction, Decode, InsertableTable, Projectable,
    ProjectionShape, QueryBuilder, SelectAst, Table, TableProjection, UpdateableTable,
};

mod query;
mod sql;

pub use query::{
    EmptyRows, TestDelete, TestInsert, TestPreparedMutation, TestPreparedSelect, TestRowReader,
    TestSelect, TestUpdate,
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

    type Insert<'conn, S, Shape, Columns, Returning>
        = TestInsert<'conn, S, Shape, Columns, Returning>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Columns: squealy::InsertAssignments,
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

    fn transaction<'conn, T, F>(
        &'conn mut self,
        f: F,
    ) -> impl std::future::Future<Output = Result<T, <Self::Backend as Backend>::Error>> + 'conn
    where
        T: 'conn,
        F: for<'tx> AsyncFnOnce(
                &'tx mut Self::Transaction<'conn>,
            ) -> Result<T, <Self::Backend as Backend>::Error>
            + 'conn,
    {
        async move {
            let mut transaction = TestConnection;
            f(&mut transaction).await
        }
    }
}
