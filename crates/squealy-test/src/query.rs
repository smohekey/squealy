use std::future::{Future, Ready, ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use squealy::{
    Backend, BindValue, Connection, Decode, Delete, DeleteQuery, Insert, InsertQuery,
    InsertableTable, ProjectionShape, RowsAffected, Select, SelectQuery, TableProjection, Update,
    UpdateQuery, UpdateableTable,
};

use crate::{TestBackend, TestConnection, TestError, sql};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmptyRows<Row> {
    _row: PhantomData<Row>,
}

impl<Row> Default for EmptyRows<Row> {
    fn default() -> Self {
        Self { _row: PhantomData }
    }
}

impl<Row> Stream for EmptyRows<Row> {
    type Item = Result<Row, TestError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

impl<Row> RowsAffected for EmptyRows<Row> {
    fn rows_affected(&self) -> Option<u64> {
        Some(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRowReader<'row> {
    _row: PhantomData<&'row ()>,
}

impl squealy::RowReader for TestRowReader<'_> {
    type Backend = TestBackend;

    fn read<T>(&mut self) -> Result<T, TestError>
    where
        T: Decode<TestBackend>,
    {
        T::decode(self)
    }
}

macro_rules! impl_test_decode_no_rows {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<TestBackend> for $ty {
            fn decode(
                _row: &mut <TestBackend as Backend>::RowReader<'_>,
            ) -> Result<Self, TestError> {
                Err(TestError::NoRows)
            }
        })*
    };
}

impl_test_decode_no_rows!(i8, i16, i32, i64, i128, isize);
impl_test_decode_no_rows!(u8, u16, u32, u64, u128, usize);
impl_test_decode_no_rows!(f32, f64);
impl_test_decode_no_rows!(String, bool);

impl<T> Decode<TestBackend> for Option<T>
where
    T: Decode<TestBackend>,
{
    fn decode(_row: &mut <TestBackend as Backend>::RowReader<'_>) -> Result<Self, TestError> {
        Ok(None)
    }
}

impl TestConnection {
    pub(crate) fn fetch_select<Row>(&self, _select: &Select) -> EmptyRows<Row> {
        empty_rows()
    }

    pub(crate) fn execute_insert(&self, _insert: &Insert) -> Ready<Result<u64, TestError>> {
        ok(0)
    }

    pub(crate) fn fetch_insert<Row>(&self, _insert: &Insert) -> EmptyRows<Row> {
        empty_rows()
    }

    pub(crate) fn execute_delete(&self, _delete: &Delete) -> Ready<Result<u64, TestError>> {
        ok(0)
    }

    pub(crate) fn fetch_delete<Row>(&self, _delete: &Delete) -> EmptyRows<Row> {
        empty_rows()
    }

    pub(crate) fn execute_update(&self, _update: &Update) -> Ready<Result<u64, TestError>> {
        ok(0)
    }

    pub(crate) fn fetch_update<Row>(&self, _update: &Update) -> EmptyRows<Row> {
        empty_rows()
    }
}

fn empty_rows<Row>() -> EmptyRows<Row> {
    EmptyRows::default()
}

fn ok<T>(value: T) -> Ready<Result<T, TestError>> {
    ready(Ok(value))
}

#[derive(Clone, Debug)]
pub struct TestSelect<'conn, Shape>
where
    Shape: ProjectionShape,
{
    connection: &'conn TestConnection,
    select: Select,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct TestInsert<'conn, S, Shape = ()>
where
    S: InsertableTable,
    Shape: ProjectionShape,
{
    connection: &'conn TestConnection,
    insert: Insert,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct TestDelete<'conn, S, Shape = ()>
where
    S: TableProjection,
    Shape: ProjectionShape,
{
    connection: &'conn TestConnection,
    delete: Delete,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct TestUpdate<'conn, S, Shape = ()>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
{
    connection: &'conn TestConnection,
    update: Update,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

impl<'conn, Shape> TestSelect<'conn, Shape>
where
    Shape: ProjectionShape,
{
    pub(crate) fn new(connection: &'conn TestConnection, select: Select) -> Self {
        Self {
            connection,
            select,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape> TestInsert<'conn, S, Shape>
where
    S: InsertableTable,
    Shape: ProjectionShape,
{
    pub(crate) fn new(connection: &'conn TestConnection, insert: Insert) -> Self {
        Self {
            connection,
            insert,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape> TestDelete<'conn, S, Shape>
where
    S: TableProjection,
    Shape: ProjectionShape,
{
    pub(crate) fn new(connection: &'conn TestConnection, delete: Delete) -> Self {
        Self {
            connection,
            delete,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape> TestUpdate<'conn, S, Shape>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
{
    pub(crate) fn new(connection: &'conn TestConnection, update: Update) -> Self {
        Self {
            connection,
            update,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, Shape> SelectQuery<'conn> for TestSelect<'conn, Shape>
where
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
{
    type Connection = TestConnection;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn ir(&self) -> &Select {
        &self.select
    }

    fn build(connection: &'conn Self::Connection, select: Select) -> Self {
        Self::new(connection, select)
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_select(&self.select)
    }
}

impl<'conn, S, Shape> InsertQuery<'conn> for TestInsert<'conn, S, Shape>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
{
    type Connection = TestConnection;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn ir(&self) -> &Insert {
        &self.insert
    }

    fn build(connection: &'conn Self::Connection, insert: Insert) -> Self {
        Self::new(connection, insert)
    }

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Connection as Connection>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        self.connection.execute_insert(&self.insert)
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_insert(&self.insert)
    }
}

impl<'conn, S, Shape> DeleteQuery<'conn> for TestDelete<'conn, S, Shape>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
{
    type Connection = TestConnection;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn ir(&self) -> &Delete {
        &self.delete
    }

    fn build(connection: &'conn Self::Connection, delete: Delete) -> Self {
        Self::new(connection, delete)
    }

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Connection as Connection>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        self.connection.execute_delete(&self.delete)
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_delete(&self.delete)
    }
}

impl<'conn, S, Shape> UpdateQuery<'conn> for TestUpdate<'conn, S, Shape>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
{
    type Connection = TestConnection;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn ir(&self) -> &Update {
        &self.update
    }

    fn build(connection: &'conn Self::Connection, update: Update) -> Self {
        Self::new(connection, update)
    }

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Connection as Connection>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        self.connection.execute_update(&self.update)
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_update(&self.update)
    }
}

impl<Shape> TestSelect<'_, Shape>
where
    Shape: ProjectionShape,
{
    pub fn to_sql(&self) -> String {
        let mut sql = Vec::new();
        sql::write_select(&self.select, &mut sql).unwrap();
        String::from_utf8(sql).unwrap()
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_select(&self.select, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        sql::select_params(&self.select)
    }
}

impl<S, Shape> TestInsert<'_, S, Shape>
where
    S: InsertableTable,
    Shape: ProjectionShape,
{
    pub fn to_sql(&self) -> String {
        let mut sql = Vec::new();
        sql::write_insert(&self.insert, &mut sql).unwrap();
        String::from_utf8(sql).unwrap()
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_insert(&self.insert, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        sql::insert_params(&self.insert)
    }
}

impl<S, Shape> TestDelete<'_, S, Shape>
where
    S: TableProjection,
    Shape: ProjectionShape,
{
    pub fn to_sql(&self) -> String {
        let mut sql = Vec::new();
        sql::write_delete(&self.delete, &mut sql).unwrap();
        String::from_utf8(sql).unwrap()
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_delete(&self.delete, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        sql::delete_params(&self.delete)
    }
}

impl<S, Shape> TestUpdate<'_, S, Shape>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
{
    pub fn to_sql(&self) -> String {
        let mut sql = Vec::new();
        sql::write_update(&self.update, &mut sql).unwrap();
        String::from_utf8(sql).unwrap()
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_update(&self.update, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        sql::update_params(&self.update)
    }
}
