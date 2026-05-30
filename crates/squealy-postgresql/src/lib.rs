use std::future::{Future, ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use squealy::{
    Backend, BindValue, Connection, Delete, DeleteQuery, Insert, InsertQuery, InsertableTable,
    ProjectionShape, Returning, Select, SelectBuilder, SelectQuery, Table, TableProjection, Update,
    UpdateQuery, UpdateableTable, build_delete, build_delete_returning, build_insert,
    build_insert_returning, build_select, build_update, build_update_returning,
};

mod sql;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PostgresConnection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostgresError {
    NoDriver,
}

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

impl<'conn, Shape> SelectQuery<'conn> for PostgresSelect<'conn, Shape>
where
    Shape: ProjectionShape,
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
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Err(PostgresError::NoDriver))
    }
}

impl<'conn, S, Shape> InsertQuery<'conn> for PostgresInsert<'conn, S, Shape>
where
    S: InsertableTable,
    Shape: ProjectionShape,
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
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Err(PostgresError::NoDriver))
    }
}

impl<'conn, S, Shape> DeleteQuery<'conn> for PostgresDelete<'conn, S, Shape>
where
    S: TableProjection,
    Shape: ProjectionShape,
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
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Err(PostgresError::NoDriver))
    }
}

impl<'conn, S, Shape> UpdateQuery<'conn> for PostgresUpdate<'conn, S, Shape>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
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
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(PostgresError::NoDriver))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Err(PostgresError::NoDriver))
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

impl Backend for PostgresConnection {
    fn write_table(
        &self,
        table: &(dyn Table + Sync),
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::write_table(table, writer)
    }
}

impl Connection for PostgresConnection {
    type Error = PostgresError;

    type Select<'conn, Shape>
        = PostgresSelect<'conn, Shape>
    where
        Self: 'conn,
        Shape: ProjectionShape;

    type Insert<'conn, S, Shape>
        = PostgresInsert<'conn, S, Shape>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape;

    type Update<'conn, S, Shape>
        = PostgresUpdate<'conn, S, Shape>
    where
        Self: 'conn,
        S: UpdateableTable,
        Shape: ProjectionShape;

    type Delete<'conn, S, Shape>
        = PostgresDelete<'conn, S, Shape>
    where
        Self: 'conn,
        S: TableProjection,
        Shape: ProjectionShape;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
    {
        PostgresSelect {
            select: build_select::<Self, Shape>(f),
            _connection: PhantomData,
            _shape: PhantomData,
        }
    }

    fn insert_query<S>(&self, columns: Vec<squealy::InsertColumn>) -> Self::Insert<'_, S, ()>
    where
        S: InsertableTable,
    {
        PostgresInsert {
            insert: build_insert::<S>(columns),
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn insert_returning_query<S, Shape>(
        &self,
        columns: Vec<squealy::InsertColumn>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Insert<'_, S, Shape>
    where
        S: InsertableTable,
        Shape: ProjectionShape,
    {
        PostgresInsert {
            insert: build_insert_returning::<S>(columns, returning),
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn update_query<S>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
    ) -> Self::Update<'_, S, ()>
    where
        S: UpdateableTable,
    {
        PostgresUpdate {
            update: build_update::<S>(alias, columns, filters),
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn update_returning_query<S, Shape>(
        &self,
        alias: String,
        columns: Vec<squealy::UpdateColumn>,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Update<'_, S, Shape>
    where
        S: UpdateableTable,
        Shape: ProjectionShape,
    {
        PostgresUpdate {
            update: build_update_returning::<S>(alias, columns, filters, returning),
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn delete_query<S>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
    ) -> Self::Delete<'_, S, ()>
    where
        S: TableProjection,
    {
        PostgresDelete {
            delete: build_delete::<S>(alias, filters),
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn delete_returning_query<S, Shape>(
        &self,
        alias: String,
        filters: Vec<squealy::Filter>,
        returning: Vec<squealy::SelectColumn>,
    ) -> Self::Delete<'_, S, Shape>
    where
        S: TableProjection,
        Shape: ProjectionShape,
    {
        PostgresDelete {
            delete: build_delete_returning::<S>(alias, filters, returning),
            _connection: PhantomData,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}
