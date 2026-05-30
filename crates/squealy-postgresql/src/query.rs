use std::future::{Future, Ready, ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use squealy::{
    BindValue, Connection, Decode, Delete, DeleteQuery, Insert, InsertQuery, InsertableTable,
    ProjectionShape, Select, SelectQuery, TableProjection, Update, UpdateQuery, UpdateableTable,
};

use crate::{PostgresConnection, PostgresError, sql};

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
    type Item = Result<Row, PostgresError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostgresRowReader<'row> {
    _row: PhantomData<&'row ()>,
}

impl squealy::RowReader for PostgresRowReader<'_> {
    type Connection = PostgresConnection;

    fn read<T>(&mut self) -> Result<T, PostgresError>
    where
        T: Decode<PostgresConnection>,
    {
        T::decode(self)
    }
}

macro_rules! impl_postgres_decode_no_driver {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<PostgresConnection> for $ty {
            fn decode(
                _row: &mut <PostgresConnection as Connection>::RowReader<'_>,
            ) -> Result<Self, PostgresError> {
                Err(PostgresError::NoDriver)
            }
        })*
    };
}

impl_postgres_decode_no_driver!(i8, i16, i32, i64, i128, isize);
impl_postgres_decode_no_driver!(u8, u16, u32, u64, u128, usize);
impl_postgres_decode_no_driver!(f32, f64);
impl_postgres_decode_no_driver!(String, bool);

impl<T> Decode<PostgresConnection> for Option<T>
where
    T: Decode<PostgresConnection>,
{
    fn decode(
        _row: &mut <PostgresConnection as Connection>::RowReader<'_>,
    ) -> Result<Self, PostgresError> {
        Err(PostgresError::NoDriver)
    }
}

fn empty_rows<Row>() -> EmptyRows<Row> {
    EmptyRows::default()
}

fn no_driver<T>() -> Ready<Result<T, PostgresError>> {
    ready(Err(PostgresError::NoDriver))
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostgresSelect<'conn, Shape>
where
    Shape: ProjectionShape,
{
    select: Select,
    _connection: PhantomData<&'conn PostgresConnection>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostgresInsert<'conn, S, Shape = ()>
where
    S: InsertableTable,
    Shape: ProjectionShape,
{
    insert: Insert,
    _connection: PhantomData<&'conn PostgresConnection>,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostgresDelete<'conn, S, Shape = ()>
where
    S: TableProjection,
    Shape: ProjectionShape,
{
    delete: Delete,
    _connection: PhantomData<&'conn PostgresConnection>,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostgresUpdate<'conn, S, Shape = ()>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
{
    update: Update,
    _connection: PhantomData<&'conn PostgresConnection>,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

impl<Shape> PostgresSelect<'_, Shape>
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

impl<S, Shape> PostgresInsert<'_, S, Shape>
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

impl<S, Shape> PostgresDelete<'_, S, Shape>
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

impl<S, Shape> PostgresUpdate<'_, S, Shape>
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

impl<'conn, Shape> SelectQuery<'conn> for PostgresSelect<'conn, Shape>
where
    Shape: ProjectionShape,
    Shape::Row: Decode<PostgresConnection>,
{
    type Connection = PostgresConnection;
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
        empty_rows()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        no_driver()
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        no_driver()
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        no_driver()
    }
}

impl<'conn, S, Shape> InsertQuery<'conn> for PostgresInsert<'conn, S, Shape>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<PostgresConnection>,
{
    type Connection = PostgresConnection;
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
        no_driver()
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        empty_rows()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        no_driver()
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        no_driver()
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        no_driver()
    }
}

impl<'conn, S, Shape> DeleteQuery<'conn> for PostgresDelete<'conn, S, Shape>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Shape::Row: Decode<PostgresConnection>,
{
    type Connection = PostgresConnection;
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
        no_driver()
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        empty_rows()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        no_driver()
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        no_driver()
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        no_driver()
    }
}

impl<'conn, S, Shape> UpdateQuery<'conn> for PostgresUpdate<'conn, S, Shape>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<PostgresConnection>,
{
    type Connection = PostgresConnection;
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
        no_driver()
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        empty_rows()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        no_driver()
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        no_driver()
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        no_driver()
    }
}

impl<Shape> PostgresSelect<'_, Shape>
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

impl<S, Shape> PostgresInsert<'_, S, Shape>
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

impl<S, Shape> PostgresDelete<'_, S, Shape>
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

impl<S, Shape> PostgresUpdate<'_, S, Shape>
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
