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
    error: Option<PostgresError>,
    _row: PhantomData<Row>,
}

impl<Row> Default for EmptyRows<Row> {
    fn default() -> Self {
        Self {
            error: None,
            _row: PhantomData,
        }
    }
}

impl<Row> Unpin for EmptyRows<Row> {}

impl<Row> Stream for EmptyRows<Row> {
    type Item = Result<Row, PostgresError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().error.take().map(Err))
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

impl PostgresConnection {
    pub(crate) fn fetch_select<Row>(&self, _select: &Select) -> EmptyRows<Row> {
        error_rows(PostgresError::NoDriver)
    }

    pub(crate) fn execute_insert(&self, _insert: &Insert) -> Ready<Result<u64, PostgresError>> {
        no_driver()
    }

    pub(crate) fn fetch_insert<Row>(&self, _insert: &Insert) -> EmptyRows<Row> {
        error_rows(PostgresError::NoDriver)
    }

    pub(crate) fn execute_delete(&self, _delete: &Delete) -> Ready<Result<u64, PostgresError>> {
        no_driver()
    }

    pub(crate) fn fetch_delete<Row>(&self, _delete: &Delete) -> EmptyRows<Row> {
        error_rows(PostgresError::NoDriver)
    }

    pub(crate) fn execute_update(&self, _update: &Update) -> Ready<Result<u64, PostgresError>> {
        no_driver()
    }

    pub(crate) fn fetch_update<Row>(&self, _update: &Update) -> EmptyRows<Row> {
        error_rows(PostgresError::NoDriver)
    }
}

fn error_rows<Row>(error: PostgresError) -> EmptyRows<Row> {
    EmptyRows {
        error: Some(error),
        _row: PhantomData,
    }
}

fn no_driver<T>() -> Ready<Result<T, PostgresError>> {
    ready(Err(PostgresError::NoDriver))
}

#[derive(Clone, Debug)]
pub struct PostgresSelect<'conn, Shape>
where
    Shape: ProjectionShape,
{
    connection: &'conn PostgresConnection,
    select: Select,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct PostgresInsert<'conn, S, Shape = ()>
where
    S: InsertableTable,
    Shape: ProjectionShape,
{
    connection: &'conn PostgresConnection,
    insert: Insert,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct PostgresDelete<'conn, S, Shape = ()>
where
    S: TableProjection,
    Shape: ProjectionShape,
{
    connection: &'conn PostgresConnection,
    delete: Delete,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct PostgresUpdate<'conn, S, Shape = ()>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
{
    connection: &'conn PostgresConnection,
    update: Update,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

impl<'conn, Shape> PostgresSelect<'conn, Shape>
where
    Shape: ProjectionShape,
{
    pub(crate) fn new(connection: &'conn PostgresConnection, select: Select) -> Self {
        Self {
            connection,
            select,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape> PostgresInsert<'conn, S, Shape>
where
    S: InsertableTable,
    Shape: ProjectionShape,
{
    pub(crate) fn new(connection: &'conn PostgresConnection, insert: Insert) -> Self {
        Self {
            connection,
            insert,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape> PostgresDelete<'conn, S, Shape>
where
    S: TableProjection,
    Shape: ProjectionShape,
{
    pub(crate) fn new(connection: &'conn PostgresConnection, delete: Delete) -> Self {
        Self {
            connection,
            delete,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape> PostgresUpdate<'conn, S, Shape>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
{
    pub(crate) fn new(connection: &'conn PostgresConnection, update: Update) -> Self {
        Self {
            connection,
            update,
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
        self.connection.fetch_select(&self.select)
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
        self.connection.execute_insert(&self.insert)
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_insert(&self.insert)
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
        self.connection.execute_delete(&self.delete)
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_delete(&self.delete)
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
        self.connection.execute_update(&self.update)
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_update(&self.update)
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
