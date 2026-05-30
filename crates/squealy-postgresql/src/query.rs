use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures_core::Stream;

use squealy::{
    Backend, BindValue, BindValueKind, Connection, Decode, Delete, DeleteQuery, FloatWidth, Insert,
    InsertQuery, InsertableTable, IntWidth, ProjectionShape, RowsAffected, Select, SelectQuery,
    TableProjection, UIntWidth, Update, UpdateQuery, UpdateableTable,
};
use tokio_postgres::types::{FromSqlOwned, IsNull, ToSql, Type, to_sql_checked};

use crate::{Postgres, PostgresConnection, PostgresError, PostgresTransaction, sql};

#[derive(Debug)]
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

#[derive(Debug)]
pub struct PostgresRowReader<'row> {
    row: &'row tokio_postgres::Row,
    index: usize,
}

impl<'row> PostgresRowReader<'row> {
    fn new(row: &'row tokio_postgres::Row) -> Self {
        Self { row, index: 0 }
    }

    fn take_sql<T>(&mut self) -> Result<T, PostgresError>
    where
        T: FromSqlOwned,
    {
        let value = self
            .row
            .try_get(self.index)
            .map_err(PostgresError::Decode)?;
        self.index += 1;
        Ok(value)
    }
}

impl squealy::RowReader for PostgresRowReader<'_> {
    type Backend = Postgres;

    fn read<T>(&mut self) -> Result<T, PostgresError>
    where
        T: Decode<Postgres>,
    {
        T::decode(self)
    }
}

macro_rules! impl_postgres_decode_direct {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<Postgres> for $ty {
            fn decode(
                row: &mut <Postgres as Backend>::RowReader<'_>,
            ) -> Result<Self, PostgresError> {
                row.take_sql()
            }
        })*
    };
}

macro_rules! impl_postgres_decode_from_i64 {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<Postgres> for $ty {
            fn decode(
                row: &mut <Postgres as Backend>::RowReader<'_>,
            ) -> Result<Self, PostgresError> {
                let value = row.take_sql::<i64>()?;
                <$ty>::try_from(value).map_err(|_| PostgresError::Conversion(stringify!($ty)))
            }
        })*
    };
}

impl_postgres_decode_direct!(i16, i32, i64, f32, f64, String, bool);
impl_postgres_decode_from_i64!(i8, i128, isize, u8, u16, u32, u64, u128, usize);

impl<T> Decode<Postgres> for Option<T>
where
    T: FromSqlOwned,
{
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        row.take_sql()
    }
}

#[doc(hidden)]
pub trait PostgresExecutor: Connection<Backend = Postgres> {
    fn decode_row<Row>(row: &tokio_postgres::Row) -> Result<Row, PostgresError>
    where
        Row: Decode<Postgres>;

    fn query_raw<'query>(
        &'query self,
        sql: String,
        params: Vec<BindValue>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    >;

    fn execute_sql<'query>(
        &'query self,
        sql: String,
        params: Vec<BindValue>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>>;
}

pub struct PostgresRows<'query, Row, Conn = PostgresConnection> {
    state: PostgresRowsState<'query>,
    affected_rows: Option<u64>,
    _row: PhantomData<Row>,
    _connection: PhantomData<fn() -> Conn>,
}

enum PostgresRowsState<'query> {
    Pending(
        Pin<
            Box<
                dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>>
                    + Send
                    + 'query,
            >,
        >,
    ),
    Rows(Pin<Box<tokio_postgres::RowStream>>),
    Done,
}

impl<'query, Row, Conn> PostgresRows<'query, Row, Conn>
where
    Conn: PostgresExecutor,
{
    fn query(connection: &'query Conn, sql: String, params: Vec<BindValue>) -> Self {
        Self {
            state: PostgresRowsState::Pending(connection.query_raw(sql, params)),
            affected_rows: None,
            _row: PhantomData,
            _connection: PhantomData,
        }
    }
}

impl<Row, Conn> Stream for PostgresRows<'_, Row, Conn>
where
    Conn: PostgresExecutor,
    Row: Decode<Postgres>,
{
    type Item = Result<Row, PostgresError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            match &mut this.state {
                PostgresRowsState::Pending(future) => {
                    let rows = match future.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(rows)) => rows,
                        Poll::Ready(Err(error)) => {
                            this.state = PostgresRowsState::Done;
                            return Poll::Ready(Some(Err(error)));
                        }
                    };
                    this.state = PostgresRowsState::Rows(Box::pin(rows));
                }
                PostgresRowsState::Rows(rows) => {
                    let row = match rows.as_mut().poll_next(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(row))) => row,
                        Poll::Ready(Some(Err(error))) => {
                            this.state = PostgresRowsState::Done;
                            return Poll::Ready(Some(Err(PostgresError::Database(error))));
                        }
                        Poll::Ready(None) => {
                            this.affected_rows = rows.rows_affected();
                            this.state = PostgresRowsState::Done;
                            return Poll::Ready(None);
                        }
                    };
                    return Poll::Ready(Some(Conn::decode_row(&row)));
                }
                PostgresRowsState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl<Row, Conn> Unpin for PostgresRows<'_, Row, Conn> {}

impl<Row, Conn> RowsAffected for PostgresRows<'_, Row, Conn> {
    fn rows_affected(&self) -> Option<u64> {
        self.affected_rows
    }
}

enum PostgresParam {
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Float32(f32),
    Float64(f64),
    Text(String),
    Bool(bool),
    Null(PostgresNull),
}

impl PostgresParam {
    fn as_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            Self::Int16(value) => value,
            Self::Int32(value) => value,
            Self::Int64(value) => value,
            Self::Float32(value) => value,
            Self::Float64(value) => value,
            Self::Text(value) => value,
            Self::Bool(value) => value,
            Self::Null(value) => value,
        }
    }
}

#[derive(Debug)]
struct PostgresNull;

impl ToSql for PostgresNull {
    fn to_sql(
        &self,
        _ty: &Type,
        _out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        Ok(IsNull::Yes)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

fn postgres_params(params: Vec<BindValue>) -> Result<Vec<PostgresParam>, PostgresError> {
    params
        .into_iter()
        .map(|param| match param.into_kind() {
            BindValueKind::Int { value, width } => postgres_signed_int(value, width),
            BindValueKind::UInt { value, width } => postgres_unsigned_int(value, width),
            BindValueKind::Float { value, width } => postgres_float(value, width),
            BindValueKind::Text(value) => Ok(PostgresParam::Text(value)),
            BindValueKind::Bool(value) => Ok(PostgresParam::Bool(value)),
            BindValueKind::Null => Ok(PostgresParam::Null(PostgresNull)),
        })
        .collect()
}

fn postgres_signed_int(value: i128, width: IntWidth) -> Result<PostgresParam, PostgresError> {
    match width {
        IntWidth::I8 | IntWidth::I16 => i16::try_from(value)
            .map(PostgresParam::Int16)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::Int(value))),
        IntWidth::I32 => i32::try_from(value)
            .map(PostgresParam::Int32)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::Int(value))),
        IntWidth::I64 | IntWidth::I128 | IntWidth::Isize => i64::try_from(value)
            .map(PostgresParam::Int64)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::Int(value))),
    }
}

fn postgres_unsigned_int(value: u128, width: UIntWidth) -> Result<PostgresParam, PostgresError> {
    match width {
        UIntWidth::U8 | UIntWidth::U16 => i32::try_from(value)
            .map(PostgresParam::Int32)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::UInt(value))),
        UIntWidth::U32 | UIntWidth::U64 | UIntWidth::U128 | UIntWidth::Usize => {
            i64::try_from(value)
                .map(PostgresParam::Int64)
                .map_err(|_| PostgresError::UnsupportedBind(BindValue::UInt(value)))
        }
    }
}

fn postgres_float(value: f64, width: FloatWidth) -> Result<PostgresParam, PostgresError> {
    match width {
        FloatWidth::F32 => Ok(PostgresParam::Float32(value as f32)),
        FloatWidth::F64 => Ok(PostgresParam::Float64(value)),
    }
}

fn render_select(select: &Select) -> String {
    let mut sql = Vec::new();
    sql::write_select(select, &mut sql).unwrap();
    String::from_utf8(sql).unwrap()
}

fn render_insert(insert: &Insert) -> String {
    let mut sql = Vec::new();
    sql::write_insert(insert, &mut sql).unwrap();
    String::from_utf8(sql).unwrap()
}

fn render_delete(delete: &Delete) -> String {
    let mut sql = Vec::new();
    sql::write_delete(delete, &mut sql).unwrap();
    String::from_utf8(sql).unwrap()
}

fn render_update(update: &Update) -> String {
    let mut sql = Vec::new();
    sql::write_update(update, &mut sql).unwrap();
    String::from_utf8(sql).unwrap()
}

impl PostgresExecutor for Postgres {
    fn decode_row<Row>(row: &tokio_postgres::Row) -> Result<Row, PostgresError>
    where
        Row: Decode<Postgres>,
    {
        let mut row = PostgresRowReader::new(row);
        Row::decode(&mut row)
    }

    fn query_raw<'query>(
        &'query self,
        _sql: String,
        _params: Vec<BindValue>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    > {
        Box::pin(async { Err(PostgresError::NoDriver) })
    }

    fn execute_sql<'query>(
        &'query self,
        _sql: String,
        _params: Vec<BindValue>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
        Box::pin(async { Err(PostgresError::NoDriver) })
    }
}

impl PostgresExecutor for PostgresConnection {
    fn decode_row<Row>(row: &tokio_postgres::Row) -> Result<Row, PostgresError>
    where
        Row: Decode<Postgres>,
    {
        let mut row = PostgresRowReader::new(row);
        Row::decode(&mut row)
    }

    fn query_raw<'query>(
        &'query self,
        sql: String,
        params: Vec<BindValue>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    > {
        let client = self.client();
        Box::pin(async move {
            let params = postgres_params(params)?;
            let params = params.iter().map(PostgresParam::as_sql).collect::<Vec<_>>();
            client
                .query_raw(&sql, params)
                .await
                .map_err(PostgresError::Database)
        })
    }

    fn execute_sql<'query>(
        &'query self,
        sql: String,
        params: Vec<BindValue>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
        let client = self.client();
        Box::pin(async move {
            let params = postgres_params(params)?;
            let params = params.iter().map(PostgresParam::as_sql).collect::<Vec<_>>();
            client
                .execute(&sql, &params)
                .await
                .map_err(PostgresError::Database)
        })
    }
}

impl PostgresExecutor for PostgresTransaction<'_> {
    fn decode_row<Row>(row: &tokio_postgres::Row) -> Result<Row, PostgresError>
    where
        Row: Decode<Postgres>,
    {
        let mut row = PostgresRowReader::new(row);
        Row::decode(&mut row)
    }

    fn query_raw<'query>(
        &'query self,
        sql: String,
        params: Vec<BindValue>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    > {
        Box::pin(async move {
            let params = postgres_params(params)?;
            let params = params.iter().map(PostgresParam::as_sql).collect::<Vec<_>>();
            self.transaction
                .query_raw(&sql, params)
                .await
                .map_err(PostgresError::Database)
        })
    }

    fn execute_sql<'query>(
        &'query self,
        sql: String,
        params: Vec<BindValue>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
        Box::pin(async move {
            let params = postgres_params(params)?;
            let params = params.iter().map(PostgresParam::as_sql).collect::<Vec<_>>();
            self.transaction
                .execute(&sql, &params)
                .await
                .map_err(PostgresError::Database)
        })
    }
}

#[derive(Clone, Debug)]
pub struct PostgresSelect<'conn, Shape, Conn = PostgresConnection>
where
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    connection: &'conn Conn,
    select: Select,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct PostgresInsert<'conn, S, Shape = (), Conn = PostgresConnection>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    connection: &'conn Conn,
    insert: Insert,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct PostgresDelete<'conn, S, Shape = (), Conn = PostgresConnection>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    connection: &'conn Conn,
    delete: Delete,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

#[derive(Clone, Debug)]
pub struct PostgresUpdate<'conn, S, Shape = (), Conn = PostgresConnection>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    connection: &'conn Conn,
    update: Update,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

impl<'conn, Shape, Conn> PostgresSelect<'conn, Shape, Conn>
where
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    pub(crate) fn new(connection: &'conn Conn, select: Select) -> Self {
        Self {
            connection,
            select,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape, Conn> PostgresInsert<'conn, S, Shape, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    pub(crate) fn new(connection: &'conn Conn, insert: Insert) -> Self {
        Self {
            connection,
            insert,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape, Conn> PostgresDelete<'conn, S, Shape, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    pub(crate) fn new(connection: &'conn Conn, delete: Delete) -> Self {
        Self {
            connection,
            delete,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape, Conn> PostgresUpdate<'conn, S, Shape, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    pub(crate) fn new(connection: &'conn Conn, update: Update) -> Self {
        Self {
            connection,
            update,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, Shape, Conn> SelectQuery<'conn> for PostgresSelect<'conn, Shape, Conn>
where
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres>,
{
    type Connection = Conn;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn ir(&self) -> &Select {
        &self.select
    }

    fn build(connection: &'conn Self::Connection, select: Select) -> Self {
        Self::new(connection, select)
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        PostgresRows::query(
            self.connection,
            render_select(&self.select),
            sql::select_params(&self.select),
        )
    }
}

impl<'conn, S, Shape, Conn> InsertQuery<'conn> for PostgresInsert<'conn, S, Shape, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres>,
{
    type Connection = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
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
        self.connection.execute_sql(
            render_insert(&self.insert),
            sql::insert_params(&self.insert),
        )
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        PostgresRows::query(
            self.connection,
            render_insert(&self.insert),
            sql::insert_params(&self.insert),
        )
    }
}

impl<'conn, S, Shape, Conn> DeleteQuery<'conn> for PostgresDelete<'conn, S, Shape, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres>,
{
    type Connection = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
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
        self.connection.execute_sql(
            render_delete(&self.delete),
            sql::delete_params(&self.delete),
        )
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        PostgresRows::query(
            self.connection,
            render_delete(&self.delete),
            sql::delete_params(&self.delete),
        )
    }
}

impl<'conn, S, Shape, Conn> UpdateQuery<'conn> for PostgresUpdate<'conn, S, Shape, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres>,
{
    type Connection = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
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
        self.connection.execute_sql(
            render_update(&self.update),
            sql::update_params(&self.update),
        )
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        PostgresRows::query(
            self.connection,
            render_update(&self.update),
            sql::update_params(&self.update),
        )
    }
}

impl<Shape, Conn> PostgresSelect<'_, Shape, Conn>
where
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    pub fn to_sql(&self) -> String {
        render_select(&self.select)
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_select(&self.select, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        sql::select_params(&self.select)
    }
}

impl<S, Shape, Conn> PostgresInsert<'_, S, Shape, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    pub fn to_sql(&self) -> String {
        render_insert(&self.insert)
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_insert(&self.insert, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        sql::insert_params(&self.insert)
    }
}

impl<S, Shape, Conn> PostgresDelete<'_, S, Shape, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    pub fn to_sql(&self) -> String {
        render_delete(&self.delete)
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_delete(&self.delete, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        sql::delete_params(&self.delete)
    }
}

impl<S, Shape, Conn> PostgresUpdate<'_, S, Shape, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor,
{
    pub fn to_sql(&self) -> String {
        render_update(&self.update)
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_update(&self.update, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        sql::update_params(&self.update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use squealy::{ColumnExpr, ColumnMode, Table};

    #[derive(Clone, Debug, PartialEq, Table)]
    struct Widget<'scope, C: ColumnMode = ColumnExpr> {
        #[column(primary_key, auto_increment, db_type = "integer")]
        id: C::Type<'scope, i32>,
        name: C::Type<'scope, String>,
    }

    #[test]
    fn signed_widths_map_to_expected_param() {
        assert!(matches!(
            postgres_signed_int(7, IntWidth::I8),
            Ok(PostgresParam::Int16(7))
        ));
        assert!(matches!(
            postgres_signed_int(7, IntWidth::I16),
            Ok(PostgresParam::Int16(7))
        ));
        assert!(matches!(
            postgres_signed_int(7, IntWidth::I32),
            Ok(PostgresParam::Int32(7))
        ));
        assert!(matches!(
            postgres_signed_int(7, IntWidth::I64),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            postgres_signed_int(7, IntWidth::I128),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            postgres_signed_int(7, IntWidth::Isize),
            Ok(PostgresParam::Int64(7))
        ));
    }

    #[test]
    fn signed_overflow_reports_unsupported_bind() {
        let too_big_for_i16 = i64::from(i16::MAX) + 1;
        assert!(matches!(
            postgres_signed_int(too_big_for_i16 as i128, IntWidth::I16),
            Err(PostgresError::UnsupportedBind(_))
        ));

        let too_big_for_i32 = i64::from(i32::MAX) + 1;
        assert!(matches!(
            postgres_signed_int(too_big_for_i32 as i128, IntWidth::I32),
            Err(PostgresError::UnsupportedBind(_))
        ));

        let too_big_for_i64 = i128::from(i64::MAX) + 1;
        assert!(matches!(
            postgres_signed_int(too_big_for_i64, IntWidth::I64),
            Err(PostgresError::UnsupportedBind(_))
        ));
    }

    #[test]
    fn unsigned_widths_map_to_expected_param() {
        assert!(matches!(
            postgres_unsigned_int(7, UIntWidth::U8),
            Ok(PostgresParam::Int32(7))
        ));
        assert!(matches!(
            postgres_unsigned_int(7, UIntWidth::U16),
            Ok(PostgresParam::Int32(7))
        ));
        assert!(matches!(
            postgres_unsigned_int(7, UIntWidth::U32),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            postgres_unsigned_int(7, UIntWidth::U64),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            postgres_unsigned_int(7, UIntWidth::U128),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            postgres_unsigned_int(7, UIntWidth::Usize),
            Ok(PostgresParam::Int64(7))
        ));
    }

    #[test]
    fn unsigned_overflow_reports_unsupported_bind() {
        let too_big_for_i32 = u64::from(u32::MAX);
        assert!(matches!(
            postgres_unsigned_int(u128::from(too_big_for_i32), UIntWidth::U16),
            Err(PostgresError::UnsupportedBind(_))
        ));

        let too_big_for_i64 = u128::from(u64::MAX);
        assert!(matches!(
            postgres_unsigned_int(too_big_for_i64, UIntWidth::U64),
            Err(PostgresError::UnsupportedBind(_))
        ));
    }

    #[test]
    fn float_widths_preserve_precision() {
        assert!(matches!(
            postgres_float(1.5, FloatWidth::F32),
            Ok(PostgresParam::Float32(value)) if value == 1.5
        ));
        assert!(matches!(
            postgres_float(1.5, FloatWidth::F64),
            Ok(PostgresParam::Float64(value)) if value == 1.5
        ));
    }

    #[test]
    fn params_pass_through_text_bool_and_null() {
        let params = postgres_params(vec![
            BindValue::text("Ada"),
            BindValue::bool(true),
            BindValue::Null,
        ])
        .expect("convert bind values");

        assert!(matches!(&params[0], PostgresParam::Text(value) if value == "Ada"));
        assert!(matches!(params[1], PostgresParam::Bool(true)));
        assert!(matches!(params[2], PostgresParam::Null(_)));
    }

    #[tokio::test]
    async fn render_backend_select_fetch_yields_no_driver_error() {
        let result = Postgres
            .select(|q| {
                let widget = q.from::<Widget>();
                q.returning(widget)
            })
            .collect()
            .await;

        assert!(matches!(result, Err(PostgresError::NoDriver)));
    }

    #[tokio::test]
    async fn render_backend_insert_execute_yields_no_driver_error() {
        let result = Postgres.insert::<Widget>().name("Ada").execute().await;

        assert!(matches!(result, Err(PostgresError::NoDriver)));
    }
}
