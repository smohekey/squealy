use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, BytesMut};
use futures_core::Stream;

use squealy::{
    Backend, BindSink, BindValue, BindValueKind, Connection, Decode, DeleteQuery,
    ExecutableDeleteQuery, ExecutableInsertQuery, ExecutableSelectQuery, ExecutableUpdateQuery,
    FloatWidth, HAppend, HList, HNil, InsertQuery, InsertRows, InsertableTable, IntWidth,
    NoRuntimeParams, PredicateNodes, PreparableDeleteQuery, PreparableInsertQuery,
    PreparableSelectQuery, PreparableUpdateQuery, PreparedMutationQuery, PreparedParamValues,
    PreparedSelectQuery, Projectable, ProjectionShape, QueryBuilder, RowsAffected, SelectAst,
    SelectQuery, Selected, SourceAlias, TableProjection, UIntWidth, UpdateQuery, UpdateableTable,
};
use tokio_postgres::{
    GenericClient,
    types::{FromSql, FromSqlOwned, IsNull, ToSql, Type, to_sql_checked},
};

use squealy::render;

use crate::sql::PostgresDialect;
use crate::{Postgres, PostgresConnection, PostgresError, PostgresTransaction};

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

macro_rules! impl_postgres_decode_from_numeric {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<Postgres> for $ty {
            fn decode(
                row: &mut <Postgres as Backend>::RowReader<'_>,
            ) -> Result<Self, PostgresError> {
                let value = row.take_sql::<PostgresNumericInteger>()?;
                <$ty>::try_from(value).map_err(|_| PostgresError::Conversion(stringify!($ty)))
            }
        })*
    };
}

impl_postgres_decode_direct!(i16, i32, i64, f32, f64, String, bool);
impl_postgres_decode_from_i64!(i8, isize, u8, u16, u32, usize);
impl_postgres_decode_from_numeric!(i128, u64, u128);

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

    fn prepare_sql<'query>(
        &'query self,
        sql: String,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::Statement, PostgresError>> + Send + 'query>,
    >;

    fn query_raw<'query>(
        &'query self,
        sql: String,
        params: Vec<PostgresParam>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    >;

    fn execute_sql<'query>(
        &'query self,
        sql: String,
        params: Vec<PostgresParam>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>>;

    fn query_statement<'query>(
        &'query self,
        statement: &'query tokio_postgres::Statement,
        params: Vec<PostgresParam>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    >;

    fn execute_statement<'query>(
        &'query self,
        statement: &'query tokio_postgres::Statement,
        params: Vec<PostgresParam>,
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
    fn query_with_params(
        connection: &'query Conn,
        sql: String,
        params: Vec<PostgresParam>,
    ) -> Self {
        Self {
            state: PostgresRowsState::Pending(connection.query_raw(sql, params)),
            affected_rows: None,
            _row: PhantomData,
            _connection: PhantomData,
        }
    }

    fn prepared(
        connection: &'query Conn,
        statement: &'query tokio_postgres::Statement,
        params: Vec<PostgresParam>,
    ) -> Self {
        Self {
            state: PostgresRowsState::Pending(connection.query_statement(statement, params)),
            affected_rows: None,
            _row: PhantomData,
            _connection: PhantomData,
        }
    }

    fn error(error: PostgresError) -> Self {
        Self {
            state: PostgresRowsState::Pending(Box::pin(std::future::ready(Err(error)))),
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

#[doc(hidden)]
pub enum PostgresParam {
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Numeric(PostgresNumericInteger),
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
            Self::Numeric(value) => value,
            Self::Float32(value) => value,
            Self::Float64(value) => value,
            Self::Text(value) => value,
            Self::Bool(value) => value,
            Self::Null(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct PostgresNumericInteger {
    negative: bool,
    magnitude: u128,
}

impl PostgresNumericInteger {
    const SIGN_POSITIVE: u16 = 0x0000;
    const SIGN_NEGATIVE: u16 = 0x4000;
    const SIGN_NAN: u16 = 0xC000;
    const SIGN_POSITIVE_INFINITY: u16 = 0xD000;
    const SIGN_NEGATIVE_INFINITY: u16 = 0xF000;
    const BASE: u128 = 10_000;
    const MAX_DIGITS: usize = 10;

    const fn unsigned(magnitude: u128) -> Self {
        Self {
            negative: false,
            magnitude,
        }
    }

    const fn signed(value: i128) -> Self {
        Self {
            negative: value.is_negative(),
            magnitude: value.unsigned_abs(),
        }
    }

    fn write_numeric(&self, out: &mut BytesMut) {
        if self.magnitude == 0 {
            out.put_i16(0);
            out.put_i16(0);
            out.put_u16(Self::SIGN_POSITIVE);
            out.put_i16(0);
            return;
        }

        let mut digits = [0u16; Self::MAX_DIGITS];
        let digits = self.write_digits(&mut digits);

        out.put_i16(digits.len() as i16);
        out.put_i16(digits.len() as i16 - 1);
        out.put_u16(if self.negative {
            Self::SIGN_NEGATIVE
        } else {
            Self::SIGN_POSITIVE
        });
        out.put_i16(0);

        for &digit in digits {
            out.put_u16(digit);
        }
    }

    fn write_digits<'digits>(
        &self,
        digits: &'digits mut [u16; Self::MAX_DIGITS],
    ) -> &'digits [u16] {
        let mut value = self.magnitude;
        let mut index = Self::MAX_DIGITS;

        while value > 0 {
            index -= 1;
            digits[index] = (value % Self::BASE) as u16;
            value /= Self::BASE;
        }

        &digits[index..]
    }
}

impl TryFrom<PostgresNumericInteger> for i128 {
    type Error = ();

    fn try_from(value: PostgresNumericInteger) -> Result<Self, Self::Error> {
        if value.negative {
            if value.magnitude == (i128::MAX as u128) + 1 {
                Ok(i128::MIN)
            } else {
                let magnitude = i128::try_from(value.magnitude).map_err(|_| ())?;
                Ok(-magnitude)
            }
        } else {
            i128::try_from(value.magnitude).map_err(|_| ())
        }
    }
}

impl TryFrom<PostgresNumericInteger> for u64 {
    type Error = ();

    fn try_from(value: PostgresNumericInteger) -> Result<Self, Self::Error> {
        if value.negative {
            Err(())
        } else {
            u64::try_from(value.magnitude).map_err(|_| ())
        }
    }
}

impl TryFrom<PostgresNumericInteger> for u128 {
    type Error = ();

    fn try_from(value: PostgresNumericInteger) -> Result<Self, Self::Error> {
        if value.negative {
            Err(())
        } else {
            Ok(value.magnitude)
        }
    }
}

impl ToSql for PostgresNumericInteger {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        if *ty == Type::FLOAT8 {
            out.put_f64(self.as_f64());
            return Ok(IsNull::No);
        }

        if *ty != Type::NUMERIC {
            return Err(format!("PostgresNumericInteger does not support SQL type {ty:?}").into());
        }

        self.write_numeric(out);
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::NUMERIC | Type::FLOAT8)
    }

    to_sql_checked!();
}

impl PostgresNumericInteger {
    fn as_f64(self) -> f64 {
        let value = self.magnitude as f64;
        if self.negative { -value } else { value }
    }
}

impl<'sql> FromSql<'sql> for PostgresNumericInteger {
    fn from_sql(ty: &Type, raw: &'sql [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        if *ty != Type::NUMERIC {
            return Err(format!("PostgresNumericInteger does not support SQL type {ty:?}").into());
        }

        let mut raw = raw;
        if raw.remaining() < 8 {
            return Err("invalid numeric value".into());
        }

        let digits_len = raw.get_i16();
        let weight = raw.get_i16();
        let sign = raw.get_u16();
        let dscale = raw.get_i16();

        if digits_len < 0 || weight < -1 || dscale < 0 {
            return Err("invalid numeric metadata".into());
        }

        let digits_len = digits_len as usize;
        if raw.remaining() != digits_len * 2 {
            return Err("invalid numeric digit length".into());
        }

        if matches!(
            sign,
            Self::SIGN_NAN | Self::SIGN_POSITIVE_INFINITY | Self::SIGN_NEGATIVE_INFINITY
        ) {
            return Err("non-finite numeric value cannot decode as integer".into());
        }
        if !matches!(sign, Self::SIGN_POSITIVE | Self::SIGN_NEGATIVE) {
            return Err("invalid numeric sign".into());
        }

        let mut magnitude = 0u128;
        for index in 0..digits_len {
            let digit = raw.get_u16();
            if digit >= Self::BASE as u16 {
                return Err("invalid numeric digit".into());
            }

            let exponent = i32::from(weight) - index as i32;
            if exponent < 0 {
                if digit != 0 {
                    return Err("fractional numeric value cannot decode as integer".into());
                }
                continue;
            }

            let place = numeric_place(exponent as u32)?;
            let digit_value = u128::from(digit)
                .checked_mul(place)
                .ok_or("numeric value exceeds u128")?;
            magnitude = magnitude
                .checked_add(digit_value)
                .ok_or("numeric value exceeds u128")?;
        }

        Ok(Self {
            negative: sign == Self::SIGN_NEGATIVE && magnitude != 0,
            magnitude,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::NUMERIC
    }
}

fn numeric_place(exponent: u32) -> Result<u128, Box<dyn Error + Sync + Send>> {
    let mut place = 1u128;
    for _ in 0..exponent {
        place = place
            .checked_mul(PostgresNumericInteger::BASE)
            .ok_or("numeric value exceeds u128")?;
    }
    Ok(place)
}

struct PostgresBindSink {
    params: Vec<PostgresParam>,
}

impl PostgresBindSink {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            params: Vec::with_capacity(capacity),
        }
    }

    fn into_params(self) -> Vec<PostgresParam> {
        self.params
    }
}

impl BindSink for PostgresBindSink {
    type Error = PostgresError;

    fn reserve_bind_values(&mut self, additional: usize) {
        self.params.reserve(additional);
    }

    fn push_bind_value(&mut self, value: BindValue) -> Result<(), Self::Error> {
        self.params.push(postgres_param(value)?);
        Ok(())
    }
}

#[derive(Debug)]
#[doc(hidden)]
pub struct PostgresNull;

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

#[cfg(test)]
fn postgres_params(params: Vec<BindValue>) -> Result<Vec<PostgresParam>, PostgresError> {
    let mut sink = PostgresBindSink::with_capacity(params.len());
    for param in params {
        sink.push_bind_value(param)?;
    }
    Ok(sink.into_params())
}

struct StringSql {
    sql: String,
}

impl StringSql {
    fn new() -> Self {
        Self { sql: String::new() }
    }

    fn into_string(self) -> String {
        self.sql
    }
}

impl std::io::Write for StringSql {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(buf).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("SQL renderer wrote non-UTF-8 bytes: {error}"),
            )
        })?;
        self.sql.push_str(text);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn rendered_sql(write: impl FnOnce(&mut StringSql) -> std::io::Result<()>) -> String {
    let mut sql = StringSql::new();
    write(&mut sql).expect("render SQL");
    sql.into_string()
}

fn collect_postgres_params(
    capacity: usize,
    write: impl FnOnce(&mut PostgresBindSink) -> Result<(), PostgresError>,
) -> Result<Vec<PostgresParam>, PostgresError> {
    let mut sink = PostgresBindSink::with_capacity(capacity);
    write(&mut sink)?;
    Ok(sink.into_params())
}

fn execute_error<'query>(
    error: PostgresError,
) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
    Box::pin(std::future::ready(Err(error)))
}

fn postgres_param(param: BindValue) -> Result<PostgresParam, PostgresError> {
    match param.into_kind() {
        BindValueKind::Int { value, width } => postgres_signed_int(value, width),
        BindValueKind::UInt { value, width } => postgres_unsigned_int(value, width),
        BindValueKind::Float { value, width } => postgres_float(value, width),
        BindValueKind::Text(value) => Ok(PostgresParam::Text(value)),
        BindValueKind::Bool(value) => Ok(PostgresParam::Bool(value)),
        BindValueKind::Null => Ok(PostgresParam::Null(PostgresNull)),
    }
}

fn resolve_prepared_params<Shape, Params>(
    bindings: &[render::SqlParam],
    params: &Params,
) -> Result<Vec<PostgresParam>, PostgresError>
where
    Shape: HList,
    Params: PreparedParamValues<Shape>,
{
    let mut sink = PostgresBindSink::with_capacity(bindings.len());
    for param in bindings {
        match param {
            render::SqlParam::Static(value) => sink.push_bind_value(value.clone())?,
            render::SqlParam::Runtime(index) => {
                if !params.write_bind_value_at(*index, &mut sink)? {
                    return Err(PostgresError::Conversion("prepared parameter"));
                }
            }
        }
    }
    Ok(sink.into_params())
}

fn postgres_signed_int(value: i128, width: IntWidth) -> Result<PostgresParam, PostgresError> {
    match width {
        IntWidth::I8 | IntWidth::I16 => i16::try_from(value)
            .map(PostgresParam::Int16)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::Int(value))),
        IntWidth::I32 => i32::try_from(value)
            .map(PostgresParam::Int32)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::Int(value))),
        IntWidth::I64 | IntWidth::Isize => i64::try_from(value)
            .map(PostgresParam::Int64)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::Int(value))),
        IntWidth::I128 => Ok(PostgresParam::Numeric(PostgresNumericInteger::signed(
            value,
        ))),
    }
}

fn postgres_unsigned_int(value: u128, width: UIntWidth) -> Result<PostgresParam, PostgresError> {
    match width {
        UIntWidth::U8 | UIntWidth::U16 => i32::try_from(value)
            .map(PostgresParam::Int32)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::UInt(value))),
        UIntWidth::U32 | UIntWidth::Usize => i64::try_from(value)
            .map(PostgresParam::Int64)
            .map_err(|_| PostgresError::UnsupportedBind(BindValue::UInt(value))),
        UIntWidth::U64 | UIntWidth::U128 => Ok(PostgresParam::Numeric(
            PostgresNumericInteger::unsigned(value),
        )),
    }
}

fn postgres_float(value: f64, width: FloatWidth) -> Result<PostgresParam, PostgresError> {
    match width {
        FloatWidth::F32 => Ok(PostgresParam::Float32(value as f32)),
        FloatWidth::F64 => Ok(PostgresParam::Float64(value)),
    }
}

async fn query_with_params<Client>(
    client: &Client,
    sql: String,
    params: Vec<PostgresParam>,
) -> Result<tokio_postgres::RowStream, PostgresError>
where
    Client: GenericClient + Sync,
{
    client
        .query_raw(&sql, params.iter().map(PostgresParam::as_sql))
        .await
        .map_err(PostgresError::Database)
}

async fn execute_with_params<Client>(
    client: &Client,
    sql: String,
    params: Vec<PostgresParam>,
) -> Result<u64, PostgresError>
where
    Client: GenericClient + Sync,
{
    client
        .execute_raw(&sql, params.iter().map(PostgresParam::as_sql))
        .await
        .map_err(PostgresError::Database)
}

async fn prepare_statement<Client>(
    client: &Client,
    sql: String,
) -> Result<tokio_postgres::Statement, PostgresError>
where
    Client: GenericClient + Sync,
{
    client.prepare(&sql).await.map_err(PostgresError::Database)
}

async fn query_prepared_with_bind_values<Client>(
    client: &Client,
    statement: &tokio_postgres::Statement,
    params: Vec<PostgresParam>,
) -> Result<tokio_postgres::RowStream, PostgresError>
where
    Client: GenericClient + Sync,
{
    client
        .query_raw(statement, params.iter().map(PostgresParam::as_sql))
        .await
        .map_err(PostgresError::Database)
}

async fn execute_prepared_with_bind_values<Client>(
    client: &Client,
    statement: &tokio_postgres::Statement,
    params: Vec<PostgresParam>,
) -> Result<u64, PostgresError>
where
    Client: GenericClient + Sync,
{
    client
        .execute_raw(statement, params.iter().map(PostgresParam::as_sql))
        .await
        .map_err(PostgresError::Database)
}

impl PostgresExecutor for PostgresConnection {
    fn decode_row<Row>(row: &tokio_postgres::Row) -> Result<Row, PostgresError>
    where
        Row: Decode<Postgres>,
    {
        let mut row = PostgresRowReader::new(row);
        Row::decode(&mut row)
    }

    fn prepare_sql<'query>(
        &'query self,
        sql: String,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::Statement, PostgresError>> + Send + 'query>,
    > {
        let client = self.client();
        Box::pin(prepare_statement(client, sql))
    }

    fn query_raw<'query>(
        &'query self,
        sql: String,
        params: Vec<PostgresParam>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    > {
        let client = self.client();
        Box::pin(query_with_params(client, sql, params))
    }

    fn execute_sql<'query>(
        &'query self,
        sql: String,
        params: Vec<PostgresParam>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
        let client = self.client();
        Box::pin(execute_with_params(client, sql, params))
    }

    fn query_statement<'query>(
        &'query self,
        statement: &'query tokio_postgres::Statement,
        params: Vec<PostgresParam>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    > {
        let client = self.client();
        Box::pin(query_prepared_with_bind_values(client, statement, params))
    }

    fn execute_statement<'query>(
        &'query self,
        statement: &'query tokio_postgres::Statement,
        params: Vec<PostgresParam>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
        let client = self.client();
        Box::pin(execute_prepared_with_bind_values(client, statement, params))
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

    fn prepare_sql<'query>(
        &'query self,
        sql: String,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::Statement, PostgresError>> + Send + 'query>,
    > {
        Box::pin(prepare_statement(&self.transaction, sql))
    }

    fn query_raw<'query>(
        &'query self,
        sql: String,
        params: Vec<PostgresParam>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    > {
        Box::pin(query_with_params(&self.transaction, sql, params))
    }

    fn execute_sql<'query>(
        &'query self,
        sql: String,
        params: Vec<PostgresParam>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
        Box::pin(execute_with_params(&self.transaction, sql, params))
    }

    fn query_statement<'query>(
        &'query self,
        statement: &'query tokio_postgres::Statement,
        params: Vec<PostgresParam>,
    ) -> Pin<
        Box<dyn Future<Output = Result<tokio_postgres::RowStream, PostgresError>> + Send + 'query>,
    > {
        Box::pin(query_prepared_with_bind_values(
            &self.transaction,
            statement,
            params,
        ))
    }

    fn execute_statement<'query>(
        &'query self,
        statement: &'query tokio_postgres::Statement,
        params: Vec<PostgresParam>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
        Box::pin(execute_prepared_with_bind_values(
            &self.transaction,
            statement,
            params,
        ))
    }
}

pub struct PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn = PostgresConnection>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    connection: &'conn Conn,
    selected: Selected<'scope, Base, Shape, Projection>,
    _shape: PhantomData<Shape>,
}

pub struct PostgresInsert<
    'conn,
    S,
    Shape = (),
    Rows = HNil,
    Returning = (),
    Conn = PostgresConnection,
> where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    connection: &'conn Conn,
    columns: Rows,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct PostgresDelete<
    'conn,
    S,
    Shape = (),
    Filters = HNil,
    Returning = (),
    Conn = PostgresConnection,
> where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    connection: &'conn Conn,
    alias: SourceAlias,
    filters: Filters,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct PostgresUpdate<
    'conn,
    S,
    Shape = (),
    Columns = HNil,
    Filters = HNil,
    Returning = (),
    Conn = PostgresConnection,
> where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    connection: &'conn Conn,
    alias: SourceAlias,
    columns: Columns,
    filters: Filters,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct PostgresPreparedSelect<'conn, Row, Conn = PostgresConnection, ParamShape = HNil>
where
    Conn: PostgresExecutor,
    ParamShape: HList,
{
    connection: &'conn Conn,
    statement: tokio_postgres::Statement,
    params: Vec<render::SqlParam>,
    _row: PhantomData<Row>,
    _params: PhantomData<ParamShape>,
}

pub struct PostgresPreparedMutation<'conn, Row, Conn = PostgresConnection, ParamShape = HNil>
where
    Conn: PostgresExecutor,
    ParamShape: HList,
{
    connection: &'conn Conn,
    statement: tokio_postgres::Statement,
    params: Vec<render::SqlParam>,
    _row: PhantomData<Row>,
    _params: PhantomData<ParamShape>,
}

impl<'conn, Row, Conn, ParamShape> PostgresPreparedSelect<'conn, Row, Conn, ParamShape>
where
    Conn: PostgresExecutor,
    ParamShape: HList,
{
    fn new(
        connection: &'conn Conn,
        statement: tokio_postgres::Statement,
        params: Vec<render::SqlParam>,
    ) -> Self {
        Self {
            connection,
            statement,
            params,
            _row: PhantomData,
            _params: PhantomData,
        }
    }
}

impl<'conn, Row, Conn, ParamShape> PostgresPreparedMutation<'conn, Row, Conn, ParamShape>
where
    Conn: PostgresExecutor,
    ParamShape: HList,
{
    fn new(
        connection: &'conn Conn,
        statement: tokio_postgres::Statement,
        params: Vec<render::SqlParam>,
    ) -> Self {
        Self {
            connection,
            statement,
            params,
            _row: PhantomData,
            _params: PhantomData,
        }
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn>
    PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    fn new_selected(
        connection: &'conn Conn,
        selected: Selected<'scope, Base, Shape, Projection>,
    ) -> Self {
        Self {
            connection,
            selected,
            _shape: PhantomData,
        }
    }

    fn prepared_sql(&self) -> render::PreparedSql {
        let mut buffer = render::PreparedSql::default();
        render::render_selected_prepared::<Conn, Base, Shape, Projection>(
            &PostgresDialect,
            &self.selected,
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError> {
        let sql = rendered_sql(|writer| {
            render::write_selected_into::<Conn, Base, Shape, Projection, _>(
                &PostgresDialect,
                &self.selected,
                writer,
            )
        });
        let params = collect_postgres_params(0, |sink| {
            render::write_selected_params::<Conn, Base, Shape, Projection, _>(
                &PostgresDialect,
                &self.selected,
                sink,
            )
        })?;
        Ok((sql, params))
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> PostgresInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    pub(crate) fn new(connection: &'conn Conn, columns: Rows, returning: Returning) -> Self {
        Self {
            connection,
            columns,
            returning,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn prepared_sql(&self) -> render::PreparedSql {
        let mut buffer = render::PreparedSql::default();
        render::render_insert_prepared::<S, _, _>(
            &PostgresDialect,
            &self.columns,
            &self.returning,
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError> {
        let sql = rendered_sql(|writer| {
            render::write_insert::<S, _, _>(
                &PostgresDialect,
                &self.columns,
                &self.returning,
                writer,
            )
        });
        let params =
            collect_postgres_params(self.columns.first_row_len() * self.columns.len(), |sink| {
                render::write_insert_params::<S, _, _, _>(
                    &PostgresDialect,
                    &self.columns,
                    &self.returning,
                    sink,
                )
            })?;
        Ok((sql, params))
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn>
    PostgresDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    pub(crate) fn new(
        connection: &'conn Conn,
        alias: SourceAlias,
        filters: Filters,
        returning: Returning,
    ) -> Self {
        Self {
            connection,
            alias,
            filters,
            returning,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn prepared_sql(&self) -> render::PreparedSql {
        let mut buffer = render::PreparedSql::default();
        render::render_delete_prepared::<S, _, _>(
            &PostgresDialect,
            self.alias,
            &self.filters,
            &self.returning,
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError> {
        let sql = rendered_sql(|writer| {
            render::write_delete::<S, _, _>(
                &PostgresDialect,
                self.alias,
                &self.filters,
                &self.returning,
                writer,
            )
        });
        let params = collect_postgres_params(self.filters.len(), |sink| {
            render::write_delete_params::<S, _, _, _>(
                &PostgresDialect,
                self.alias,
                &self.filters,
                &self.returning,
                sink,
            )
        })?;
        Ok((sql, params))
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    pub(crate) fn new(
        connection: &'conn Conn,
        alias: SourceAlias,
        columns: Columns,
        filters: Filters,
        returning: Returning,
    ) -> Self {
        Self {
            connection,
            alias,
            columns,
            filters,
            returning,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn prepared_sql(&self) -> render::PreparedSql {
        let mut buffer = render::PreparedSql::default();
        render::render_update_prepared::<S, _, _, _>(
            &PostgresDialect,
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError> {
        let sql = rendered_sql(|writer| {
            render::write_update::<S, _, _, _>(
                &PostgresDialect,
                self.alias,
                &self.columns,
                &self.filters,
                &self.returning,
                writer,
            )
        });
        let params = collect_postgres_params(self.columns.len() + self.filters.len(), |sink| {
            render::write_update_params::<S, _, _, _, _>(
                &PostgresDialect,
                self.alias,
                &self.columns,
                &self.filters,
                &self.returning,
                sink,
            )
        })?;
        Ok((sql, params))
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> SelectQuery<'conn, 'scope, Base, Projection>
    for PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
    Shape::Row: Decode<Postgres>,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
{
    type Builder = Conn;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build_selected(
        connection: &'conn Self::Builder,
        selected: Selected<'scope, Base, Self::Shape, Projection>,
    ) -> Self {
        Self::new_selected(connection, selected)
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn>
    ExecutableSelectQuery<'conn, 'scope, Base, Projection>
    for PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres>,
    Base: SelectAst<'conn, 'scope, Conn>,
    Base::Params: NoRuntimeParams,
    Projection: Projectable,
{
    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn fetch(&self) -> Self::RowStream<'_> {
        match self.execution_parts() {
            Ok((sql, params)) => PostgresRows::query_with_params(self.connection, sql, params),
            Err(error) => PostgresRows::error(error),
        }
    }
}

impl<'conn, Row, Conn, ParamShape> PreparedSelectQuery<'conn>
    for PostgresPreparedSelect<'conn, Row, Conn, ParamShape>
where
    Row: Decode<Postgres> + Send,
    Conn: PostgresExecutor + 'conn,
    ParamShape: HList,
{
    type Builder = Conn;
    type Params = ParamShape;
    type Row = Row;

    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn fetch<'query, ParamValues>(&'query self, params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues: PreparedParamValues<Self::Params>,
    {
        match resolve_prepared_params::<ParamShape, _>(&self.params, &params) {
            Ok(params) => PostgresRows::prepared(self.connection, &self.statement, params),
            Err(error) => PostgresRows::error(error),
        }
    }
}

impl<'conn, Row, Conn, ParamShape> PreparedMutationQuery<'conn>
    for PostgresPreparedMutation<'conn, Row, Conn, ParamShape>
where
    Row: Decode<Postgres> + Send,
    Conn: PostgresExecutor + 'conn,
    ParamShape: HList,
{
    type Builder = Conn;
    type Params = ParamShape;
    type Row = Row;

    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<
        Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>,
    > + Send
    + 'query
    where
        'conn: 'query,
        ParamValues: PreparedParamValues<Self::Params> + 'query,
    {
        match resolve_prepared_params::<ParamShape, _>(&self.params, &params) {
            Ok(params) => self.connection.execute_statement(&self.statement, params),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }

    fn fetch<'query, ParamValues>(&'query self, params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues: PreparedParamValues<Self::Params>,
    {
        match resolve_prepared_params::<ParamShape, _>(&self.params, &params) {
            Ok(params) => PostgresRows::prepared(self.connection, &self.statement, params),
            Err(error) => PostgresRows::error(error),
        }
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn>
    PreparableSelectQuery<'conn, 'scope, Base, Projection>
    for PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres> + Send,
    Base: SelectAst<'conn, 'scope, Conn>,
    Base::Params: HList,
    Projection: Projectable,
{
    type Params = Base::Params;

    type Prepared<'prepared>
        = PostgresPreparedSelect<'prepared, Shape::Row, Conn, Base::Params>
    where
        Self: 'prepared,
        'conn: 'prepared,
        'scope: 'prepared,
        Base: 'prepared,
        Projection: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<
        Output = Result<
            Self::Prepared<'prepared>,
            <<Self::Builder as QueryBuilder>::Backend as Backend>::Error,
        >,
    > + 'prepared
    where
        'conn: 'prepared,
        'scope: 'prepared,
        Base: 'prepared,
        Projection: 'prepared,
    {
        let (sql, params) = self.prepared_sql().into_parts();
        async move {
            let statement = self.connection.prepare_sql(sql).await?;
            Ok(PostgresPreparedSelect::new(
                self.connection,
                statement,
                params,
            ))
        }
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> InsertQuery<'conn, Rows, Returning>
    for PostgresInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
    Shape::Row: Decode<Postgres>,
    Rows: InsertRows,
    Returning: Projectable,
{
    type Builder = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(connection: &'conn Self::Builder, columns: Rows, returning: Returning) -> Self {
        Self::new(connection, columns, returning)
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> ExecutableInsertQuery<'conn, Rows, Returning>
    for PostgresInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres>,
    Rows: InsertRows,
    Rows::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.execute_sql(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self.execution_parts() {
            Ok((sql, params)) => PostgresRows::query_with_params(self.connection, sql, params),
            Err(error) => PostgresRows::error(error),
        }
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> PreparableInsertQuery<'conn, Rows, Returning>
    for PostgresInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres> + Send,
    Rows: InsertRows,
    Rows::Params: HList,
    Returning: Projectable,
{
    type Params = Rows::Params;

    type Prepared<'prepared>
        = PostgresPreparedMutation<'prepared, Shape::Row, Conn, Rows::Params>
    where
        Self: 'prepared,
        'conn: 'prepared,
        Rows: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<
        Output = Result<
            Self::Prepared<'prepared>,
            <<Self::Builder as QueryBuilder>::Backend as Backend>::Error,
        >,
    > + 'prepared
    where
        'conn: 'prepared,
        Rows: 'prepared,
        Returning: 'prepared,
    {
        let (sql, params) = self.prepared_sql().into_parts();
        async move {
            let statement = self.connection.prepare_sql(sql).await?;
            Ok(PostgresPreparedMutation::new(
                self.connection,
                statement,
                params,
            ))
        }
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn> DeleteQuery<'conn, Filters, Returning>
    for PostgresDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
    Shape::Row: Decode<Postgres>,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Builder = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(
        connection: &'conn Self::Builder,
        alias: SourceAlias,
        filters: Filters,
        returning: Returning,
    ) -> Self {
        Self::new(connection, alias, filters, returning)
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn> ExecutableDeleteQuery<'conn, Filters, Returning>
    for PostgresDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres>,
    Filters: PredicateNodes,
    Filters::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.execute_sql(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self.execution_parts() {
            Ok((sql, params)) => PostgresRows::query_with_params(self.connection, sql, params),
            Err(error) => PostgresRows::error(error),
        }
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn> PreparableDeleteQuery<'conn, Filters, Returning>
    for PostgresDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres> + Send,
    Filters: PredicateNodes,
    Filters::Params: HList,
    Returning: Projectable,
{
    type Params = Filters::Params;

    type Prepared<'prepared>
        = PostgresPreparedMutation<'prepared, Shape::Row, Conn, Filters::Params>
    where
        Self: 'prepared,
        'conn: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<
        Output = Result<
            Self::Prepared<'prepared>,
            <<Self::Builder as QueryBuilder>::Backend as Backend>::Error,
        >,
    > + 'prepared
    where
        'conn: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared,
    {
        let (sql, params) = self.prepared_sql().into_parts();
        async move {
            let statement = self.connection.prepare_sql(sql).await?;
            Ok(PostgresPreparedMutation::new(
                self.connection,
                statement,
                params,
            ))
        }
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    UpdateQuery<'conn, Columns, Filters, Returning>
    for PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
    Shape::Row: Decode<Postgres>,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Builder = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(
        connection: &'conn Self::Builder,
        alias: SourceAlias,
        columns: Columns,
        filters: Filters,
        returning: Returning,
    ) -> Self {
        Self::new(connection, alias, columns, filters, returning)
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    ExecutableUpdateQuery<'conn, Columns, Filters, Returning>
    for PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres>,
    Columns: squealy::UpdateAssignments,
    Columns::Params: NoRuntimeParams,
    Filters: PredicateNodes,
    Filters::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>
        = PostgresRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.execute_sql(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self.execution_parts() {
            Ok((sql, params)) => PostgresRows::query_with_params(self.connection, sql, params),
            Err(error) => PostgresRows::error(error),
        }
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    PreparableUpdateQuery<'conn, Columns, Filters, Returning>
    for PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: PostgresExecutor + 'conn,
    Shape::Row: Decode<Postgres> + Send,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Columns::Params: HAppend<Filters::Params>,
    <Columns::Params as HAppend<Filters::Params>>::Output: HList,
    Returning: Projectable,
{
    type Params = <Columns::Params as HAppend<Filters::Params>>::Output;

    type Prepared<'prepared>
        = PostgresPreparedMutation<
        'prepared,
        Shape::Row,
        Conn,
        <Columns::Params as HAppend<Filters::Params>>::Output,
    >
    where
        Self: 'prepared,
        'conn: 'prepared,
        Columns: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<
        Output = Result<
            Self::Prepared<'prepared>,
            <<Self::Builder as QueryBuilder>::Backend as Backend>::Error,
        >,
    > + 'prepared
    where
        'conn: 'prepared,
        Columns: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared,
    {
        let (sql, params) = self.prepared_sql().into_parts();
        async move {
            let statement = self.connection.prepare_sql(sql).await?;
            Ok(PostgresPreparedMutation::new(
                self.connection,
                statement,
                params,
            ))
        }
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn>
    PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Postgres>,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
{
    /// Render this query into a newly allocated SQL string.
    ///
    /// Use [`Self::write_sql`] to stream SQL into caller-provided storage instead.
    pub fn to_sql(&self) -> String {
        rendered_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        render::write_selected_into::<Conn, Base, Shape, Projection, _>(
            &PostgresDialect,
            &self.selected,
            writer,
        )
    }

    /// Write bind parameters into a caller-provided sink.
    pub fn write_params<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: BindSink,
    {
        render::write_selected_params::<Conn, Base, Shape, Projection, _>(
            &PostgresDialect,
            &self.selected,
            sink,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Vec<BindValue> {
        let mut params = Vec::new();
        self.write_params(&mut params)
            .unwrap_or_else(|error| match error {});
        params
    }
}

impl<S, Shape, Rows, Returning, Conn> PostgresInsert<'_, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    /// Render this query into a newly allocated SQL string.
    ///
    /// Use [`Self::write_sql`] to stream SQL into caller-provided storage instead.
    pub fn to_sql(&self) -> String {
        rendered_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        render::write_insert::<S, _, _>(&PostgresDialect, &self.columns, &self.returning, writer)
    }

    /// Write bind parameters into a caller-provided sink.
    pub fn write_params<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: BindSink,
    {
        render::write_insert_params::<S, _, _, _>(
            &PostgresDialect,
            &self.columns,
            &self.returning,
            sink,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Vec<BindValue> {
        let mut params = Vec::new();
        self.write_params(&mut params)
            .unwrap_or_else(|error| match error {});
        params
    }
}

impl<S, Shape, Filters, Returning, Conn> PostgresDelete<'_, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    /// Render this query into a newly allocated SQL string.
    ///
    /// Use [`Self::write_sql`] to stream SQL into caller-provided storage instead.
    pub fn to_sql(&self) -> String {
        rendered_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        render::write_delete::<S, _, _>(
            &PostgresDialect,
            self.alias,
            &self.filters,
            &self.returning,
            writer,
        )
    }

    /// Write bind parameters into a caller-provided sink.
    pub fn write_params<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: BindSink,
    {
        render::write_delete_params::<S, _, _, _>(
            &PostgresDialect,
            self.alias,
            &self.filters,
            &self.returning,
            sink,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Vec<BindValue> {
        let mut params = Vec::new();
        self.write_params(&mut params)
            .unwrap_or_else(|error| match error {});
        params
    }
}

impl<S, Shape, Columns, Filters, Returning, Conn>
    PostgresUpdate<'_, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Postgres>,
{
    /// Render this query into a newly allocated SQL string.
    ///
    /// Use [`Self::write_sql`] to stream SQL into caller-provided storage instead.
    pub fn to_sql(&self) -> String {
        rendered_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        render::write_update::<S, _, _, _>(
            &PostgresDialect,
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            writer,
        )
    }

    /// Write bind parameters into a caller-provided sink.
    pub fn write_params<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: BindSink,
    {
        render::write_update_params::<S, _, _, _, _>(
            &PostgresDialect,
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            sink,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Vec<BindValue> {
        let mut params = Vec::new();
        self.write_params(&mut params)
            .unwrap_or_else(|error| match error {});
        params
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Ok(PostgresParam::Numeric(PostgresNumericInteger {
                negative: false,
                magnitude: 7
            }))
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
            Ok(PostgresParam::Numeric(PostgresNumericInteger {
                negative: false,
                magnitude: 7
            }))
        ));
        assert!(matches!(
            postgres_unsigned_int(7, UIntWidth::U128),
            Ok(PostgresParam::Numeric(PostgresNumericInteger {
                negative: false,
                magnitude: 7
            }))
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

        assert!(matches!(
            postgres_unsigned_int(u128::from(u64::MAX), UIntWidth::U64),
            Ok(PostgresParam::Numeric(PostgresNumericInteger {
                negative: false,
                magnitude
            })) if magnitude == u128::from(u64::MAX)
        ));
    }

    #[test]
    fn numeric_integer_round_trips_wide_values() {
        let values = [
            PostgresNumericInteger::signed(i128::MIN),
            PostgresNumericInteger::signed(i128::MAX),
            PostgresNumericInteger::unsigned(u128::MAX),
            PostgresNumericInteger::signed(-123_456_789_012_345_678_901_234_567_890i128),
            PostgresNumericInteger::unsigned(123_456_789_012_345_678_901_234_567_890u128),
        ];

        for value in values {
            let mut bytes = BytesMut::new();
            value.write_numeric(&mut bytes);
            let decoded =
                PostgresNumericInteger::from_sql(&Type::NUMERIC, &bytes).expect("decode numeric");

            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn numeric_integer_rejects_fractional_values() {
        let mut bytes = BytesMut::new();
        bytes.put_i16(2);
        bytes.put_i16(0);
        bytes.put_u16(PostgresNumericInteger::SIGN_POSITIVE);
        bytes.put_i16(4);
        bytes.put_u16(1);
        bytes.put_u16(5_000);

        assert!(PostgresNumericInteger::from_sql(&Type::NUMERIC, &bytes).is_err());
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
}
