use std::collections::VecDeque;
use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures_core::Stream;

use squealy::{
    BindValue, BindValueKind, Connection, Decode, Delete, DeleteQuery, FloatWidth, Insert,
    InsertQuery, InsertableTable, IntWidth, ProjectionShape, Select, SelectQuery, TableProjection,
    UIntWidth, Update, UpdateQuery, UpdateableTable,
};
use tokio_postgres::types::{FromSqlOwned, IsNull, ToSql, Type, to_sql_checked};

use crate::{PostgresConnection, PostgresError, sql};

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

    fn take<T>(&mut self) -> Result<T, PostgresError>
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
    type Connection = PostgresConnection;

    fn read<T>(&mut self) -> Result<T, PostgresError>
    where
        T: Decode<PostgresConnection>,
    {
        T::decode(self)
    }
}

macro_rules! impl_postgres_decode_direct {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<PostgresConnection> for $ty {
            fn decode(
                row: &mut <PostgresConnection as Connection>::RowReader<'_>,
            ) -> Result<Self, PostgresError> {
                row.take()
            }
        })*
    };
}

macro_rules! impl_postgres_decode_from_i64 {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<PostgresConnection> for $ty {
            fn decode(
                row: &mut <PostgresConnection as Connection>::RowReader<'_>,
            ) -> Result<Self, PostgresError> {
                let value = row.take::<i64>()?;
                <$ty>::try_from(value).map_err(|_| PostgresError::Conversion(stringify!($ty)))
            }
        })*
    };
}

impl_postgres_decode_direct!(i16, i32, i64, f32, f64, String, bool);
impl_postgres_decode_from_i64!(i8, i128, isize, u8, u16, u32, u64, u128, usize);

impl<T> Decode<PostgresConnection> for Option<T>
where
    T: FromSqlOwned,
{
    fn decode(
        row: &mut <PostgresConnection as Connection>::RowReader<'_>,
    ) -> Result<Self, PostgresError> {
        row.take()
    }
}

pub struct PostgresRows<'query, Row> {
    state: PostgresRowsState<'query>,
    _row: PhantomData<Row>,
}

enum PostgresRowsState<'query> {
    Pending(
        Pin<
            Box<
                dyn Future<Output = Result<VecDeque<tokio_postgres::Row>, PostgresError>>
                    + Send
                    + 'query,
            >,
        >,
    ),
    Rows(VecDeque<tokio_postgres::Row>),
    Error(Option<PostgresError>),
    Done,
}

impl<Row> PostgresRows<'_, Row> {
    fn error(error: PostgresError) -> Self {
        Self {
            state: PostgresRowsState::Error(Some(error)),
            _row: PhantomData,
        }
    }
}

impl<'query, Row> PostgresRows<'query, Row> {
    fn query(
        client: Result<&'query tokio_postgres::Client, PostgresError>,
        sql: String,
        params: Vec<BindValue>,
    ) -> Self {
        let Ok(client) = client else {
            return Self::error(PostgresError::NoDriver);
        };

        Self {
            state: PostgresRowsState::Pending(Box::pin(async move {
                let params = postgres_params(params)?;
                let params = params.iter().map(PostgresParam::as_sql).collect::<Vec<_>>();
                client
                    .query(&sql, &params)
                    .await
                    .map(VecDeque::from)
                    .map_err(PostgresError::Database)
            })),
            _row: PhantomData,
        }
    }
}

impl<Row> Stream for PostgresRows<'_, Row>
where
    Row: Decode<PostgresConnection>,
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
                    this.state = PostgresRowsState::Rows(rows);
                }
                PostgresRowsState::Rows(rows) => {
                    let Some(row) = rows.pop_front() else {
                        this.state = PostgresRowsState::Done;
                        return Poll::Ready(None);
                    };
                    let mut row = PostgresRowReader::new(&row);
                    return Poll::Ready(Some(Row::decode(&mut row)));
                }
                PostgresRowsState::Error(error) => {
                    return Poll::Ready(error.take().map(Err));
                }
                PostgresRowsState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl<Row> Unpin for PostgresRows<'_, Row> {}

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

async fn execute_sql(
    client: Result<&tokio_postgres::Client, PostgresError>,
    sql: String,
    params: Vec<BindValue>,
) -> Result<u64, PostgresError> {
    let client = client?;
    let params = postgres_params(params)?;
    let params = params.iter().map(PostgresParam::as_sql).collect::<Vec<_>>();
    client
        .execute(&sql, &params)
        .await
        .map_err(PostgresError::Database)
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

impl PostgresConnection {
    pub(crate) fn fetch_select<Row>(&self, select: &Select) -> PostgresRows<'_, Row> {
        PostgresRows::query(
            self.client(),
            render_select(select),
            sql::select_params(select),
        )
    }

    pub(crate) fn execute_insert(
        &self,
        insert: &Insert,
    ) -> impl Future<Output = Result<u64, PostgresError>> + Send + '_ {
        execute_sql(
            self.client(),
            render_insert(insert),
            sql::insert_params(insert),
        )
    }

    pub(crate) fn fetch_insert<Row>(&self, insert: &Insert) -> PostgresRows<'_, Row> {
        PostgresRows::query(
            self.client(),
            render_insert(insert),
            sql::insert_params(insert),
        )
    }

    pub(crate) fn execute_delete(
        &self,
        delete: &Delete,
    ) -> impl Future<Output = Result<u64, PostgresError>> + Send + '_ {
        execute_sql(
            self.client(),
            render_delete(delete),
            sql::delete_params(delete),
        )
    }

    pub(crate) fn fetch_delete<Row>(&self, delete: &Delete) -> PostgresRows<'_, Row> {
        PostgresRows::query(
            self.client(),
            render_delete(delete),
            sql::delete_params(delete),
        )
    }

    pub(crate) fn execute_update(
        &self,
        update: &Update,
    ) -> impl Future<Output = Result<u64, PostgresError>> + Send + '_ {
        execute_sql(
            self.client(),
            render_update(update),
            sql::update_params(update),
        )
    }

    pub(crate) fn fetch_update<Row>(&self, update: &Update) -> PostgresRows<'_, Row> {
        PostgresRows::query(
            self.client(),
            render_update(update),
            sql::update_params(update),
        )
    }
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
        = PostgresRows<'query, Self::Row>
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
        = PostgresRows<'query, Self::Row>
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
        = PostgresRows<'query, Self::Row>
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
        = PostgresRows<'query, Self::Row>
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
        render_select(&self.select)
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
        render_insert(&self.insert)
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
        render_delete(&self.delete)
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
    async fn select_without_driver_yields_no_driver_error() {
        let connection = PostgresConnection::no_driver();
        let result = connection
            .select(|q| {
                let widget = q.from::<Widget>();
                q.returning(widget)
            })
            .fetch_all()
            .await;

        assert!(matches!(result, Err(PostgresError::NoDriver)));
    }

    #[tokio::test]
    async fn insert_without_driver_yields_no_driver_error() {
        let connection = PostgresConnection::no_driver();
        let result = connection.insert::<Widget>().name("Ada").execute().await;

        assert!(matches!(result, Err(PostgresError::NoDriver)));
    }
}
