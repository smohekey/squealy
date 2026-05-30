use std::future::{Future, ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use squealy::{
    BindValue, Connection, Delete, DeleteQuery, Insert, InsertQuery, InsertableTable,
    ProjectionShape, Select, SelectQuery, TableProjection, Update, UpdateQuery, UpdateableTable,
};

use crate::{TestConnection, TestError, sql};

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

#[derive(Clone, Debug, PartialEq)]
pub struct TestSelect<'conn, Shape>
where
    Shape: ProjectionShape,
{
    select: Select,
    _connection: PhantomData<&'conn TestConnection>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestInsert<'conn, S, Shape = ()>
where
    S: InsertableTable,
    Shape: ProjectionShape,
{
    insert: Insert,
    _connection: PhantomData<&'conn TestConnection>,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestDelete<'conn, S, Shape = ()>
where
    S: TableProjection,
    Shape: ProjectionShape,
{
    delete: Delete,
    _connection: PhantomData<&'conn TestConnection>,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestUpdate<'conn, S, Shape = ()>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
{
    update: Update,
    _connection: PhantomData<&'conn TestConnection>,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

impl<Shape> TestSelect<'_, Shape>
where
    Shape: ProjectionShape,
{
    pub(crate) fn new(select: Select) -> Self {
        Self {
            select,
            _connection: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<S, Shape> TestInsert<'_, S, Shape>
where
    S: InsertableTable,
    Shape: ProjectionShape,
{
    pub(crate) fn new(insert: Insert) -> Self {
        Self {
            insert,
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<S, Shape> TestDelete<'_, S, Shape>
where
    S: TableProjection,
    Shape: ProjectionShape,
{
    pub(crate) fn new(delete: Delete) -> Self {
        Self {
            delete,
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<S, Shape> TestUpdate<'_, S, Shape>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
{
    pub(crate) fn new(update: Update) -> Self {
        Self {
            update,
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, Shape> SelectQuery<'conn> for TestSelect<'conn, Shape>
where
    Shape: ProjectionShape,
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

    fn fetch(&self) -> Self::RowStream<'_> {
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Ok(Vec::new()))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(TestError::NoRows))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Ok(None))
    }
}

impl<'conn, S, Shape> InsertQuery<'conn> for TestInsert<'conn, S, Shape>
where
    S: InsertableTable,
    Shape: ProjectionShape,
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

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Ok(0))
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Ok(Vec::new()))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(TestError::NoRows))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Ok(None))
    }
}

impl<'conn, S, Shape> DeleteQuery<'conn> for TestDelete<'conn, S, Shape>
where
    S: TableProjection,
    Shape: ProjectionShape,
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

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Ok(0))
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Ok(Vec::new()))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(TestError::NoRows))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Ok(None))
    }
}

impl<'conn, S, Shape> UpdateQuery<'conn> for TestUpdate<'conn, S, Shape>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
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

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Ok(0))
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Ok(Vec::new()))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(TestError::NoRows))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Ok(None))
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
