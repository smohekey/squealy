use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, BytesMut};
use futures_core::Stream;

use squealy::{
    Backend, Connection, Decode, DeleteQuery, DeleteUsingQuery, Encode, ExecutableDeleteQuery,
    ExecutableDeleteUsingQuery, ExecutableInsertQuery, ExecutableSelectQuery,
    ExecutableUpdateFromQuery, ExecutableUpdateQuery, HAppend, HList, HNil, InsertQuery,
    InsertRows, InsertableTable, IntoInsertSelect, NoRuntimeParams, ParamWriter, PredicateNodes,
    PreparableDeleteQuery, PreparableInsertQuery, PreparableSelectQuery, PreparableUpdateQuery,
    PreparedMutationQuery, PreparedParamValues, PreparedSelectQuery, Projectable, ProjectionShape,
    QueryBuilder, RenderInsertRows, RenderPredicateNodes, RenderProjectable, RenderSelectAst,
    RenderUpdateAssignments, RowsAffected, SchemaTable, SelectAst, SelectQuery, Selected, SetArm,
    SetLeaf, SetOperand, SetOperations, SetSelectModifiers, SetTail, SourceAlias, TableProjection,
    UpdateAssignments, UpdateFromQuery, UpdateQuery, UpdateableTable,
};
use squealy::{ExecutableSetSelectQuery, PreparableSetSelectQuery};
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

    /// Decode a possibly-`NULL` value by peeking the current column without consuming it: a SQL
    /// `NULL` yields `None` (and skips the column), otherwise the inner [`Decode`] reads it. Decoding
    /// through `Decode for T` (rather than `Option<T>: FromSqlOwned`) lets nullable `ColumnType`
    /// newtype columns project, since those carry `Decode` but not `FromSqlOwned`.
    fn take_nullable<T>(&mut self) -> Result<Option<T>, PostgresError>
    where
        T: Decode<Postgres>,
    {
        // `NullProbe` accepts any column type, so this peek works regardless of `T`. `try_get` does
        // not advance our cursor, so a present value is then read by `T::decode` at the same index.
        let probe: Option<NullProbe> = self
            .row
            .try_get(self.index)
            .map_err(PostgresError::Decode)?;
        if probe.is_none() {
            self.index += 1;
            Ok(None)
        } else {
            T::decode(self).map(Some)
        }
    }
}

/// A `FromSql` adapter used only to test the current column for SQL `NULL`: it accepts any column
/// type and discards the bytes, so `Option<NullProbe>` reports presence without decoding a value.
struct NullProbe;

impl tokio_postgres::types::FromSql<'_> for NullProbe {
    fn from_sql(
        _ty: &tokio_postgres::types::Type,
        _raw: &[u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(NullProbe)
    }

    fn accepts(_ty: &tokio_postgres::types::Type) -> bool {
        true
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

impl_postgres_decode_direct!(i16, i32, i64, f32, f64, String, bool, Vec<u8>);
impl_postgres_decode_from_i64!(i8, isize, u8, u16, u32, usize);
impl_postgres_decode_from_numeric!(i128, u64, u128);

impl<T> Decode<Postgres> for Option<T>
where
    T: Decode<Postgres>,
{
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        row.take_nullable()
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

#[derive(Clone, Debug, PartialEq)]
#[doc(hidden)]
pub enum PostgresParam {
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Numeric(PostgresNumericInteger),
    Float32(f32),
    Float64(f64),
    Text(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Null(PostgresNull),
    #[cfg(feature = "uuid")]
    Uuid(uuid::Uuid),
    #[cfg(feature = "serde")]
    Json(serde_json::Value),
    #[cfg(feature = "systemtime")]
    SystemTime(std::time::SystemTime),
    #[cfg(feature = "time")]
    TimeTimestamp(time::OffsetDateTime),
    #[cfg(feature = "chrono")]
    ChronoTimestamp(chrono::DateTime<chrono::Utc>),
}

/// Encodes a single value into one native [`PostgresParam`] via [`Encode`], asserting the
/// one-literal-one-parameter invariant the renderer relies on. Used by the codec unit tests.
#[cfg(test)]
pub(crate) fn encode_to_param<T>(value: &T) -> Result<PostgresParam, PostgresError>
where
    T: Encode<Postgres>,
{
    let mut params = Vec::with_capacity(1);
    value.encode(&mut PostgresParamWriter::new(&mut params))?;
    let mut params = params.into_iter();
    let param = params
        .next()
        .ok_or(PostgresError::Conversion("bind produced no parameter"))?;
    if params.next().is_some() {
        return Err(PostgresError::Conversion(
            "bind produced more than one parameter",
        ));
    }
    Ok(param)
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
            Self::Bytes(value) => value,
            Self::Bool(value) => value,
            Self::Null(value) => value,
            #[cfg(feature = "uuid")]
            Self::Uuid(value) => value,
            #[cfg(feature = "serde")]
            Self::Json(value) => value,
            #[cfg(feature = "systemtime")]
            Self::SystemTime(value) => value,
            #[cfg(feature = "time")]
            Self::TimeTimestamp(value) => value,
            #[cfg(feature = "chrono")]
            Self::ChronoTimestamp(value) => value,
        }
    }
}

/// A serde-backed `jsonb` column wrapper.
///
/// Any `T: Serialize + DeserializeOwned` stored as `Json<T>` encodes to and decodes from a
/// native PostgreSQL `jsonb` column. This is the backend-local serde stack — the wrapper
/// lives here (not in core) because Rust's orphan rule forbids a shared `core::Json<T>` with
/// a per-backend `Encode<Postgres>` impl, and because the physical storage (`jsonb`) is
/// backend-specific.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Json<T>(pub T);

#[cfg(feature = "serde")]
impl<T> squealy::HasColumnType for Json<T> {
    const COLUMN_TYPE: squealy::ColumnType = squealy::ColumnType::Jsonb;
}

// `Json<T>` is a non-null column value type; `Option<Json<T>>` is the nullable form.
#[cfg(feature = "serde")]
impl<T> squealy::ColumnNullability for Json<T> {
    type Inner = Self;
    type Nullability = squealy::NonNullableColumn;
    const NULLABLE: bool = false;
}

#[cfg(feature = "serde")]
impl<T> squealy::ExprKind for Json<T> {
    type Value = Self;
}

#[cfg(feature = "serde")]
impl<T> Encode<Postgres> for Json<T>
where
    T: serde::Serialize,
{
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        let value = serde_json::to_value(&self.0)
            .map_err(|_| PostgresError::Conversion("serialize jsonb"))?;
        out.push(PostgresParam::Json(value));
        Ok(())
    }
}

#[cfg(feature = "serde")]
impl<T> Decode<Postgres> for Json<T>
where
    T: serde::de::DeserializeOwned,
{
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        let value = row.take_sql::<serde_json::Value>()?;
        let inner = serde_json::from_value(value)
            .map_err(|_| PostgresError::Conversion("deserialize jsonb"))?;
        Ok(Json(inner))
    }
}

/// Native `uuid` column support: `uuid::Uuid` encodes to and decodes from a real
/// PostgreSQL `uuid` column. A bare `uuid::Uuid` field maps to a `uuid` column and can be used as a
/// query-builder value directly (the core `uuid` feature supplies `HasColumnType` + `ExprKind`); a
/// `#[derive(ColumnType)]` newtype over `Uuid` works too.
#[cfg(feature = "uuid")]
impl Encode<Postgres> for uuid::Uuid {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::Uuid(*self));
        Ok(())
    }
}

#[cfg(feature = "uuid")]
impl Decode<Postgres> for uuid::Uuid {
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        row.take_sql()
    }
}

/// Native timestamp column support: each type encodes to and decodes from a real PostgreSQL
/// `timestamptz` column via tokio-postgres's built-in (`SystemTime`) or feature-gated
/// (`time` / `chrono`) `ToSql`/`FromSql` bridges.
#[cfg(feature = "systemtime")]
impl Encode<Postgres> for std::time::SystemTime {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::SystemTime(*self));
        Ok(())
    }
}

#[cfg(feature = "systemtime")]
impl Decode<Postgres> for std::time::SystemTime {
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        row.take_sql()
    }
}

#[cfg(feature = "time")]
impl Encode<Postgres> for time::OffsetDateTime {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::TimeTimestamp(*self));
        Ok(())
    }
}

#[cfg(feature = "time")]
impl Decode<Postgres> for time::OffsetDateTime {
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        row.take_sql()
    }
}

#[cfg(feature = "chrono")]
impl Encode<Postgres> for chrono::DateTime<chrono::Utc> {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::ChronoTimestamp(*self));
        Ok(())
    }
}

#[cfg(feature = "chrono")]
impl Decode<Postgres> for chrono::DateTime<chrono::Utc> {
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        row.take_sql()
    }
}

/// Encode-side mirror of [`PostgresRowReader`]: appends native [`PostgresParam`]s.
#[doc(hidden)]
pub struct PostgresParamWriter<'param> {
    params: &'param mut Vec<PostgresParam>,
}

impl<'param> PostgresParamWriter<'param> {
    pub(crate) fn new(params: &'param mut Vec<PostgresParam>) -> Self {
        Self { params }
    }

    pub fn push(&mut self, param: PostgresParam) {
        self.params.push(param);
    }

    pub fn push_null(&mut self) {
        self.params.push(PostgresParam::Null(PostgresNull));
    }
}

impl ParamWriter for PostgresParamWriter<'_> {
    type Backend = Postgres;

    fn write<T>(&mut self, value: &T) -> Result<(), PostgresError>
    where
        T: Encode<Postgres>,
    {
        value.encode(self)
    }
}

macro_rules! impl_postgres_encode {
    ($($ty:ty => |$value:ident| $param:expr),* $(,)?) => {
        $(impl Encode<Postgres> for $ty {
            fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
                let $value = self;
                out.push($param);
                Ok(())
            }
        })*
    };
}

impl_postgres_encode! {
    i8 => |v| PostgresParam::Int16(i16::from(*v)),
    i16 => |v| PostgresParam::Int16(*v),
    i32 => |v| PostgresParam::Int32(*v),
    i64 => |v| PostgresParam::Int64(*v),
    isize => |v| PostgresParam::Int64(*v as i64),
    i128 => |v| PostgresParam::Numeric(PostgresNumericInteger::signed(*v)),
    u8 => |v| PostgresParam::Int32(i32::from(*v)),
    u16 => |v| PostgresParam::Int32(i32::from(*v)),
    u32 => |v| PostgresParam::Int64(i64::from(*v)),
    u64 => |v| PostgresParam::Numeric(PostgresNumericInteger::unsigned(u128::from(*v))),
    u128 => |v| PostgresParam::Numeric(PostgresNumericInteger::unsigned(*v)),
    f32 => |v| PostgresParam::Float32(*v),
    f64 => |v| PostgresParam::Float64(*v),
    bool => |v| PostgresParam::Bool(*v),
}

impl Encode<Postgres> for usize {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        let value = i64::try_from(*self).map_err(|_| PostgresError::Conversion("usize"))?;
        out.push(PostgresParam::Int64(value));
        Ok(())
    }
}

impl Encode<Postgres> for str {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::Text(self.to_owned()));
        Ok(())
    }
}

impl Encode<Postgres> for String {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::Text(self.clone()));
        Ok(())
    }
}

impl Encode<Postgres> for Vec<u8> {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::Bytes(self.clone()));
        Ok(())
    }
}

// Fixed-size byte arrays bind as `bytea` (the fixed width is enforced by the column's CHECK and on
// decode here). Encode sends the bytes; decode reads the `bytea` and errors unless it is exactly `N`.
impl<const N: usize> Encode<Postgres> for [u8; N] {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::Bytes(self.to_vec()));
        Ok(())
    }
}

impl<const N: usize> Decode<Postgres> for [u8; N] {
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        let bytes = row.take_sql::<Vec<u8>>()?;
        <[u8; N]>::try_from(bytes).map_err(|_| PostgresError::Conversion("fixed-size byte array"))
    }
}

// `bytes::Bytes` binds to a `bytea` column, going through `Vec<u8>` so no extra tokio-postgres
// feature is needed (behind the opt-in `bytes` feature).
#[cfg(feature = "bytes")]
impl Encode<Postgres> for bytes::Bytes {
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        out.push(PostgresParam::Bytes(self.to_vec()));
        Ok(())
    }
}

#[cfg(feature = "bytes")]
impl Decode<Postgres> for bytes::Bytes {
    fn decode(row: &mut <Postgres as Backend>::RowReader<'_>) -> Result<Self, PostgresError> {
        Ok(bytes::Bytes::from(row.take_sql::<Vec<u8>>()?))
    }
}

impl<T> Encode<Postgres> for Option<T>
where
    T: Encode<Postgres>,
{
    fn encode(&self, out: &mut PostgresParamWriter<'_>) -> Result<(), PostgresError> {
        match self {
            Some(value) => value.encode(out),
            None => {
                out.push_null();
                Ok(())
            }
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    write: impl FnOnce(&mut Vec<PostgresParam>) -> Result<(), PostgresError>,
) -> Result<Vec<PostgresParam>, PostgresError> {
    let mut params = Vec::with_capacity(capacity);
    write(&mut params)?;
    Ok(params)
}

fn execute_error<'query>(
    error: PostgresError,
) -> Pin<Box<dyn Future<Output = Result<u64, PostgresError>> + Send + 'query>> {
    Box::pin(std::future::ready(Err(error)))
}

fn resolve_prepared_params<Shape, Params>(
    bindings: &[render::SqlParam<Postgres>],
    params: &Params,
) -> Result<Vec<PostgresParam>, PostgresError>
where
    Shape: HList,
    Params: PreparedParamValues<Shape, Postgres>,
{
    let mut values = Vec::with_capacity(bindings.len());
    for binding in bindings {
        match binding {
            render::SqlParam::Static(param) => values.push(param.clone()),
            render::SqlParam::Runtime(index) => {
                let mut writer = PostgresParamWriter::new(&mut values);
                if !params.write_param_at(*index, &mut writer)? {
                    return Err(PostgresError::Conversion("prepared parameter"));
                }
            }
        }
    }
    Ok(values)
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
    /// `Some` for an upsert (`ON CONFLICT …`); `None` for a plain insert. Carries no bind params.
    conflict: Option<squealy::ConflictClause>,
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
    params: Vec<render::SqlParam<Postgres>>,
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
    params: Vec<render::SqlParam<Postgres>>,
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
        params: Vec<render::SqlParam<Postgres>>,
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
        params: Vec<render::SqlParam<Postgres>>,
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

    fn prepared_sql(&self) -> render::PreparedSql<Postgres>
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Postgres>,
        Projection: RenderProjectable<Postgres>,
    {
        let mut buffer = render::PreparedSql::default();
        render::render_selected_prepared::<Conn, Base, Shape, Projection>(
            &PostgresDialect,
            &self.selected,
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError>
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Postgres>,
        Projection: RenderProjectable<Postgres>,
    {
        let sql = rendered_sql(|writer| {
            render::write_selected_into::<Conn, Base, Shape, Projection, _>(
                &PostgresDialect,
                &self.selected,
                writer,
            )
        });
        let params = collect_postgres_params(0, |params| {
            render::write_selected_params::<Conn, Base, Shape, Projection>(
                &PostgresDialect,
                &self.selected,
                params,
            )
        })?;
        Ok((sql, params))
    }
}

// ---------------------------------------------------------------------------
// Set operations
// ---------------------------------------------------------------------------

/// A set-operation query object (`(<left>) UNION (<right>) …`) over a [`SetArm`] tree.
pub struct PostgresSetSelect<'conn, 'scope, Tree, Conn = PostgresConnection>
where
    Conn: QueryBuilder<Backend = Postgres>,
{
    connection: &'conn Conn,
    tree: Tree,
    tail: SetTail,
    _scope: PhantomData<&'scope ()>,
}

impl<'conn, 'scope, Tree, Conn> PostgresSetSelect<'conn, 'scope, Tree, Conn>
where
    Conn: QueryBuilder<Backend = Postgres>,
{
    fn new(connection: &'conn Conn, tree: Tree) -> Self {
        Self {
            connection,
            tree,
            tail: SetTail::default(),
            _scope: PhantomData,
        }
    }
}

impl<'conn, 'scope, Tree, Conn> PostgresSetSelect<'conn, 'scope, Tree, Conn>
where
    Conn: QueryBuilder<Backend = Postgres>,
    Tree: render::RenderSetArm<'conn, 'scope, Conn, Postgres>,
{
    fn prepared_sql(&self) -> render::PreparedSql<Postgres> {
        let mut buffer = render::PreparedSql::default();
        render::render_set_prepared::<Conn, Tree>(
            &PostgresDialect,
            &self.tree,
            &self.tail,
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError> {
        let sql = rendered_sql(|writer| {
            render::write_set_into::<Conn, Tree, _>(
                &PostgresDialect,
                &self.tree,
                &self.tail,
                writer,
            )
        });
        let params = collect_postgres_params(0, |params| {
            render::write_set_params::<Conn, Tree>(&PostgresDialect, &self.tree, &self.tail, params)
        })?;
        Ok((sql, params))
    }

    /// Render this set query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        rendered_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        render::write_set_into::<Conn, Tree, _>(&PostgresDialect, &self.tree, &self.tail, writer)
    }

    /// Collect bind parameters (left-to-right across the whole tree) into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<PostgresParam>, PostgresError> {
        let mut params = Vec::new();
        render::write_set_params::<Conn, Tree>(
            &PostgresDialect,
            &self.tree,
            &self.tail,
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, 'scope, Tree, Conn> ExecutableSetSelectQuery<'conn>
    for PostgresSetSelect<'conn, 'scope, Tree, Conn>
where
    Conn: PostgresExecutor + 'conn,
    Tree: render::RenderSetArm<'conn, 'scope, Conn, Postgres>,
    <Tree as SetArm<'conn, 'scope, Conn>>::Row: Decode<Postgres> + Send,
    // As for a plain select, executing requires the query carry no runtime params: the one-shot
    // execution path inlines literals, so a `param()` in any arm would emit an unbindable placeholder.
    <Tree as SetArm<'conn, 'scope, Conn>>::Params: NoRuntimeParams,
{
    type Builder = Conn;
    type Row = <Tree as SetArm<'conn, 'scope, Conn>>::Row;

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

impl<'conn, 'scope, Tree, Conn> PreparableSetSelectQuery<'conn>
    for PostgresSetSelect<'conn, 'scope, Tree, Conn>
where
    Conn: PostgresExecutor + 'conn,
    Tree: render::RenderSetArm<'conn, 'scope, Conn, Postgres>,
    <Tree as SetArm<'conn, 'scope, Conn>>::Row: Decode<Postgres> + Send,
    <Tree as SetArm<'conn, 'scope, Conn>>::Params: HList,
{
    type Builder = Conn;
    type Params = <Tree as SetArm<'conn, 'scope, Conn>>::Params;
    type Row = <Tree as SetArm<'conn, 'scope, Conn>>::Row;

    type Prepared<'prepared>
        = PostgresPreparedSelect<'prepared, Self::Row, Conn, Self::Params>
    where
        Self: 'prepared,
        'conn: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<Output = Result<Self::Prepared<'prepared>, PostgresError>> + 'prepared
    where
        'conn: 'prepared,
    {
        let prepared = self.prepared_sql().into_parts();
        async move {
            let (sql, params) = prepared?;
            let statement = self.connection.prepare_sql(sql).await?;
            Ok(PostgresPreparedSelect::new(
                self.connection,
                statement,
                params,
            ))
        }
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> SetOperand<'conn, 'scope, Conn>
    for PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    // A row-locked select cannot be a set operand — PostgreSQL rejects a locking clause in any
    // `UNION`/`INTERSECT`/`EXCEPT` input. (`SetOperations` requires `SetOperand`, so this also blocks a
    // locked select as the left operand.)
    Base: SelectAst<'conn, 'scope, Conn, RowLockState = squealy::RowUnlocked>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    type Row = Shape::Row;
    type Arm = SetLeaf<'conn, 'scope, Conn, Base, Shape, Projection>;

    fn into_set_parts(self) -> (&'conn Conn, Self::Arm) {
        (self.connection, SetLeaf::new(self.selected))
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> IntoInsertSelect<'conn, 'scope, Conn>
    for PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    // Any row-lock state — a locked single select renders `INSERT … SELECT … FOR UPDATE` (valid on
    // PostgreSQL). The lock ban applies only to set-op operands, via their `SetOperand` impls.
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    type Row = Shape::Row;

    type InsertSelectQuery<S, Returning>
        = PostgresInsertSelect<
        'conn,
        'scope,
        S,
        SetLeaf<'conn, 'scope, Conn, Base, Shape, Projection>,
        Returning,
        Conn,
    >
    where
        S: InsertableTable,
        Returning: Projectable;

    fn into_insert_select<S, Returning>(
        self,
        connection: &'conn Conn,
        columns: Vec<&'static str>,
        returning: Returning,
    ) -> Self::InsertSelectQuery<S, Returning>
    where
        S: InsertableTable,
        Returning: Projectable,
    {
        PostgresInsertSelect::new(connection, columns, SetLeaf::new(self.selected), returning)
    }
}

// A set-op source (`select(...).union(...)`, etc.) inserts as `INSERT INTO t (cols) SELECT … UNION …`.
// Its `SetOperand::Arm` is a `SetGroup` carrying the set tree plus any trailing `ORDER BY`/`LIMIT`, so
// `into_set_parts` preserves the tail.
impl<'conn, 'scope, Tree, Conn> IntoInsertSelect<'conn, 'scope, Conn>
    for PostgresSetSelect<'conn, 'scope, Tree, Conn>
where
    Tree: SetArm<'conn, 'scope, Conn>,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    type Row = <Tree as SetArm<'conn, 'scope, Conn>>::Row;

    type InsertSelectQuery<S, Returning>
        = PostgresInsertSelect<'conn, 'scope, S, squealy::SetGroup<Tree>, Returning, Conn>
    where
        S: InsertableTable,
        Returning: Projectable;

    fn into_insert_select<S, Returning>(
        self,
        connection: &'conn Conn,
        columns: Vec<&'static str>,
        returning: Returning,
    ) -> Self::InsertSelectQuery<S, Returning>
    where
        S: InsertableTable,
        Returning: Projectable,
    {
        // Use the *destination* `connection`; the source contributes only its set arm (with its tail).
        let (_source_connection, arm) = self.into_set_parts();
        PostgresInsertSelect::new(connection, columns, arm, returning)
    }
}

/// `INSERT INTO t (columns) <select>` query object (PostgreSQL).
pub struct PostgresInsertSelect<'conn, 'scope, S, Tree, Returning, Conn = PostgresConnection> {
    connection: &'conn Conn,
    columns: Vec<&'static str>,
    source: Tree,
    returning: Returning,
    _table: PhantomData<S>,
    _scope: PhantomData<&'scope ()>,
}

impl<'conn, 'scope, S, Tree, Returning, Conn>
    PostgresInsertSelect<'conn, 'scope, S, Tree, Returning, Conn>
{
    fn new(
        connection: &'conn Conn,
        columns: Vec<&'static str>,
        source: Tree,
        returning: Returning,
    ) -> Self {
        Self {
            connection,
            columns,
            source,
            returning,
            _table: PhantomData,
            _scope: PhantomData,
        }
    }
}

impl<'conn, 'scope, S, Tree, Returning, Conn>
    PostgresInsertSelect<'conn, 'scope, S, Tree, Returning, Conn>
where
    S: InsertableTable,
    Tree: render::RenderSetArm<'conn, 'scope, Conn, Postgres>,
    Returning: RenderProjectable<Postgres>,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError> {
        let sql = rendered_sql(|writer| {
            render::write_insert_select::<S, Conn, _, _>(
                &PostgresDialect,
                &self.columns,
                &self.source,
                &self.returning,
                writer,
            )
        });
        let params = collect_postgres_params(0, |params| {
            render::write_insert_select_params::<S, Conn, _, _>(
                &PostgresDialect,
                &self.columns,
                &self.source,
                &self.returning,
                params,
            )
        })?;
        Ok((sql, params))
    }

    /// Render this `INSERT … SELECT` into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        rendered_sql(|writer| {
            render::write_insert_select::<S, Conn, _, _>(
                &PostgresDialect,
                &self.columns,
                &self.source,
                &self.returning,
                writer,
            )
        })
    }

    /// Execute the insert, returning the number of rows affected.
    pub fn insert(&self) -> impl Future<Output = Result<u64, PostgresError>> + Send + '_
    where
        Conn: PostgresExecutor,
        // One-shot execution collects only static binds, so the source must be free of runtime
        // `param`s (a runtime-parameterized source would leave a placeholder with no value).
        <Tree as SetArm<'conn, 'scope, Conn>>::Params: NoRuntimeParams,
    {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.execute_sql(sql, params),
            Err(error) => execute_error(error),
        }
    }
}

/// Correlated `UPDATE … FROM` query object (PostgreSQL).
pub struct PostgresUpdateFrom<
    'conn,
    S,
    O,
    Columns = HNil,
    Filters = HNil,
    Conn = PostgresConnection,
> {
    connection: &'conn Conn,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    columns: Columns,
    filters: Filters,
    _table: PhantomData<(S, O)>,
}

impl<'conn, S, O, Columns, Filters, Conn> PostgresUpdateFrom<'conn, S, O, Columns, Filters, Conn>
where
    S: UpdateableTable,
    O: SchemaTable,
    Columns: RenderUpdateAssignments<Postgres>,
    Filters: RenderPredicateNodes<Postgres>,
{
    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError> {
        let sql = self.to_sql();
        let params = collect_postgres_params(0, |params| {
            render::write_update_from_params::<S, O, Postgres, _, _, _>(
                &PostgresDialect,
                self.target_alias,
                self.source_alias,
                &self.columns,
                &self.filters,
                &(),
                params,
            )
        })?;
        Ok((sql, params))
    }

    /// Render this correlated update into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        rendered_sql(|writer| {
            render::write_update_from::<S, O, Postgres, _, _, _>(
                &PostgresDialect,
                self.target_alias,
                self.source_alias,
                &self.columns,
                &self.filters,
                &(),
                writer,
            )
        })
    }

    /// Collect bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<PostgresParam>, PostgresError> {
        let mut params = Vec::new();
        render::write_update_from_params::<S, O, Postgres, _, _, _>(
            &PostgresDialect,
            self.target_alias,
            self.source_alias,
            &self.columns,
            &self.filters,
            &(),
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, S, O, Columns, Filters, Conn> UpdateFromQuery<'conn, S, O, Columns, Filters>
    for PostgresUpdateFrom<'conn, S, O, Columns, Filters, Conn>
where
    S: UpdateableTable,
    O: SchemaTable,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    type Builder = Conn;

    fn build(
        connection: &'conn Conn,
        target_alias: SourceAlias,
        source_alias: SourceAlias,
        columns: Columns,
        filters: Filters,
    ) -> Self {
        Self {
            connection,
            target_alias,
            source_alias,
            columns,
            filters,
            _table: PhantomData,
        }
    }
}

impl<'conn, S, O, Columns, Filters, Conn> ExecutableUpdateFromQuery<'conn, S, O, Columns, Filters>
    for PostgresUpdateFrom<'conn, S, O, Columns, Filters, Conn>
where
    S: UpdateableTable,
    O: SchemaTable,
    Columns: RenderUpdateAssignments<Postgres>,
    Columns::Params: NoRuntimeParams,
    Filters: RenderPredicateNodes<Postgres>,
    Filters::Params: NoRuntimeParams,
    Conn: PostgresExecutor + 'conn,
{
    fn execute(&self) -> impl Future<Output = Result<u64, PostgresError>> + Send + '_ {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.execute_sql(sql, params),
            Err(error) => execute_error(error),
        }
    }
}

/// Correlated `DELETE … USING` query object (PostgreSQL).
pub struct PostgresDeleteUsing<'conn, S, O, Filters = HNil, Conn = PostgresConnection> {
    connection: &'conn Conn,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    filters: Filters,
    _table: PhantomData<(S, O)>,
}

impl<'conn, S, O, Filters, Conn> PostgresDeleteUsing<'conn, S, O, Filters, Conn>
where
    S: TableProjection,
    O: TableProjection,
    Filters: RenderPredicateNodes<Postgres>,
{
    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError> {
        let sql = self.to_sql();
        let params = collect_postgres_params(0, |params| {
            render::write_delete_using_params::<S, O, Postgres, _, _>(
                &PostgresDialect,
                self.target_alias,
                self.source_alias,
                &self.filters,
                &(),
                params,
            )
        })?;
        Ok((sql, params))
    }

    /// Render this correlated delete into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        rendered_sql(|writer| {
            render::write_delete_using::<S, O, Postgres, _, _>(
                &PostgresDialect,
                self.target_alias,
                self.source_alias,
                &self.filters,
                &(),
                writer,
            )
        })
    }

    /// Collect bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<PostgresParam>, PostgresError> {
        let mut params = Vec::new();
        render::write_delete_using_params::<S, O, Postgres, _, _>(
            &PostgresDialect,
            self.target_alias,
            self.source_alias,
            &self.filters,
            &(),
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, S, O, Filters, Conn> DeleteUsingQuery<'conn, S, O, Filters>
    for PostgresDeleteUsing<'conn, S, O, Filters, Conn>
where
    S: TableProjection,
    O: TableProjection,
    Filters: PredicateNodes,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    type Builder = Conn;

    fn build(
        connection: &'conn Conn,
        target_alias: SourceAlias,
        source_alias: SourceAlias,
        filters: Filters,
    ) -> Self {
        Self {
            connection,
            target_alias,
            source_alias,
            filters,
            _table: PhantomData,
        }
    }
}

impl<'conn, S, O, Filters, Conn> ExecutableDeleteUsingQuery<'conn, S, O, Filters>
    for PostgresDeleteUsing<'conn, S, O, Filters, Conn>
where
    S: TableProjection + UpdateableTable,
    O: TableProjection,
    Filters: RenderPredicateNodes<Postgres>,
    Filters::Params: NoRuntimeParams,
    Conn: PostgresExecutor + 'conn,
{
    fn execute(&self) -> impl Future<Output = Result<u64, PostgresError>> + Send + '_ {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.execute_sql(sql, params),
            Err(error) => execute_error(error),
        }
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> SetOperations<'conn, 'scope, Conn>
    for PostgresSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    // Matches the `SetOperand` supertrait bound: a row-locked select cannot start a set operation.
    Base: SelectAst<'conn, 'scope, Conn, RowLockState = squealy::RowUnlocked>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    type SetSelect<Tree>
        = PostgresSetSelect<'conn, 'scope, Tree, Conn>
    where
        Tree: SetArm<'conn, 'scope, Conn>;

    fn make_set_select<Tree>(connection: &'conn Conn, tree: Tree) -> Self::SetSelect<Tree>
    where
        Tree: SetArm<'conn, 'scope, Conn>,
    {
        PostgresSetSelect::new(connection, tree)
    }
}

impl<'conn, 'scope, Tree, Conn> SetOperand<'conn, 'scope, Conn>
    for PostgresSetSelect<'conn, 'scope, Tree, Conn>
where
    Tree: SetArm<'conn, 'scope, Conn>,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    type Row = <Tree as SetArm<'conn, 'scope, Conn>>::Row;
    type Arm = squealy::SetGroup<Tree>;

    fn into_set_parts(self) -> (&'conn Conn, Self::Arm) {
        (
            self.connection,
            squealy::SetGroup::new(self.tree, self.tail),
        )
    }
}

impl<'conn, 'scope, Tree, Conn> SetOperations<'conn, 'scope, Conn>
    for PostgresSetSelect<'conn, 'scope, Tree, Conn>
where
    Tree: SetArm<'conn, 'scope, Conn>,
    Conn: QueryBuilder<Backend = Postgres> + 'conn,
{
    type SetSelect<NewTree>
        = PostgresSetSelect<'conn, 'scope, NewTree, Conn>
    where
        NewTree: SetArm<'conn, 'scope, Conn>;

    fn make_set_select<NewTree>(connection: &'conn Conn, tree: NewTree) -> Self::SetSelect<NewTree>
    where
        NewTree: SetArm<'conn, 'scope, Conn>,
    {
        PostgresSetSelect::new(connection, tree)
    }
}

impl<'conn, 'scope, Tree, Conn> SetSelectModifiers<'scope>
    for PostgresSetSelect<'conn, 'scope, Tree, Conn>
where
    Tree: SetArm<'conn, 'scope, Conn>,
    Conn: QueryBuilder<Backend = Postgres>,
{
    type Shape = <Tree as SetArm<'conn, 'scope, Conn>>::Shape;

    fn set_tail_mut(&mut self) -> &mut SetTail {
        &mut self.tail
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
            conflict: None,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    pub(crate) fn new_upsert(
        connection: &'conn Conn,
        columns: Rows,
        returning: Returning,
        conflict: squealy::ConflictClause,
    ) -> Self {
        Self {
            connection,
            columns,
            returning,
            conflict: Some(conflict),
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn prepared_sql(&self) -> render::PreparedSql<Postgres>
    where
        Rows: RenderInsertRows<Postgres>,
        Returning: RenderProjectable<Postgres>,
    {
        let mut buffer = render::PreparedSql::default();
        render::render_insert_prepared::<S, Postgres, _, _>(
            &PostgresDialect,
            &self.columns,
            &self.returning,
            self.conflict.as_ref(),
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError>
    where
        Rows: RenderInsertRows<Postgres>,
        Returning: RenderProjectable<Postgres>,
    {
        let sql = rendered_sql(|writer| {
            render::write_insert::<S, Postgres, _, _>(
                &PostgresDialect,
                &self.columns,
                &self.returning,
                self.conflict.as_ref(),
                writer,
            )
        });
        let params = collect_postgres_params(
            self.columns.first_row_len() * self.columns.len(),
            |params| {
                render::write_insert_params::<S, Postgres, _, _>(
                    &PostgresDialect,
                    &self.columns,
                    &self.returning,
                    self.conflict.as_ref(),
                    params,
                )
            },
        )?;
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

    fn prepared_sql(&self) -> render::PreparedSql<Postgres>
    where
        Filters: RenderPredicateNodes<Postgres>,
        Returning: RenderProjectable<Postgres>,
    {
        let mut buffer = render::PreparedSql::default();
        render::render_delete_prepared::<S, Postgres, _, _>(
            &PostgresDialect,
            self.alias,
            &self.filters,
            &self.returning,
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError>
    where
        Filters: RenderPredicateNodes<Postgres>,
        Returning: RenderProjectable<Postgres>,
    {
        let sql = rendered_sql(|writer| {
            render::write_delete::<S, Postgres, _, _>(
                &PostgresDialect,
                self.alias,
                &self.filters,
                &self.returning,
                writer,
            )
        });
        let params = collect_postgres_params(self.filters.len(), |params| {
            render::write_delete_params::<S, Postgres, _, _>(
                &PostgresDialect,
                self.alias,
                &self.filters,
                &self.returning,
                params,
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

    fn prepared_sql(&self) -> render::PreparedSql<Postgres>
    where
        Columns: RenderUpdateAssignments<Postgres>,
        Filters: RenderPredicateNodes<Postgres>,
        Returning: RenderProjectable<Postgres>,
    {
        let mut buffer = render::PreparedSql::default();
        render::render_update_prepared::<S, Postgres, _, _, _>(
            &PostgresDialect,
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            &mut buffer,
        );
        buffer
    }

    fn execution_parts(&self) -> Result<(String, Vec<PostgresParam>), PostgresError>
    where
        Columns: RenderUpdateAssignments<Postgres>,
        Filters: RenderPredicateNodes<Postgres>,
        Returning: RenderProjectable<Postgres>,
    {
        let sql = rendered_sql(|writer| {
            render::write_update::<S, Postgres, _, _, _>(
                &PostgresDialect,
                self.alias,
                &self.columns,
                &self.filters,
                &self.returning,
                writer,
            )
        });
        let params = collect_postgres_params(self.columns.len() + self.filters.len(), |params| {
            render::write_update_params::<S, Postgres, _, _, _>(
                &PostgresDialect,
                self.alias,
                &self.columns,
                &self.filters,
                &self.returning,
                params,
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
    Base: RenderSelectAst<'conn, 'scope, Conn, Postgres>,
    Base::Params: NoRuntimeParams,
    Projection: RenderProjectable<Postgres>,
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
        ParamValues: PreparedParamValues<Self::Params, Postgres>,
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
        ParamValues: PreparedParamValues<Self::Params, Postgres> + 'query,
    {
        match resolve_prepared_params::<ParamShape, _>(&self.params, &params) {
            Ok(params) => self.connection.execute_statement(&self.statement, params),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }

    fn fetch<'query, ParamValues>(&'query self, params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues: PreparedParamValues<Self::Params, Postgres>,
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
    Base: RenderSelectAst<'conn, 'scope, Conn, Postgres>,
    Base::Params: HList,
    Projection: RenderProjectable<Postgres>,
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
        let prepared = self.prepared_sql().into_parts();
        async move {
            let (sql, params) = prepared?;
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
    Rows: RenderInsertRows<Postgres>,
    Rows::Params: NoRuntimeParams,
    Returning: RenderProjectable<Postgres>,
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
    Rows: RenderInsertRows<Postgres>,
    Rows::Params: HList,
    Returning: RenderProjectable<Postgres>,
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
        let prepared = self.prepared_sql().into_parts();
        async move {
            let (sql, params) = prepared?;
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
    Filters: RenderPredicateNodes<Postgres>,
    Filters::Params: NoRuntimeParams,
    Returning: RenderProjectable<Postgres>,
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
    Filters: RenderPredicateNodes<Postgres>,
    Filters::Params: HList,
    Returning: RenderProjectable<Postgres>,
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
        let prepared = self.prepared_sql().into_parts();
        async move {
            let (sql, params) = prepared?;
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
    Columns: RenderUpdateAssignments<Postgres>,
    Columns::Params: NoRuntimeParams,
    Filters: RenderPredicateNodes<Postgres>,
    Filters::Params: NoRuntimeParams,
    Returning: RenderProjectable<Postgres>,
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
    Columns: RenderUpdateAssignments<Postgres>,
    Filters: RenderPredicateNodes<Postgres>,
    Columns::Params: HAppend<Filters::Params>,
    <Columns::Params as HAppend<Filters::Params>>::Output: HList,
    Returning: RenderProjectable<Postgres>,
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
        let prepared = self.prepared_sql().into_parts();
        async move {
            let (sql, params) = prepared?;
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
    Base: RenderSelectAst<'conn, 'scope, Conn, Postgres>,
    Projection: RenderProjectable<Postgres>,
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

    /// Write bind parameters into a caller-provided native param vector.
    pub fn write_params(&self, params: &mut Vec<PostgresParam>) -> Result<(), PostgresError> {
        render::write_selected_params::<Conn, Base, Shape, Projection>(
            &PostgresDialect,
            &self.selected,
            params,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Result<Vec<PostgresParam>, PostgresError> {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}

impl<S, Shape, Rows, Returning, Conn> PostgresInsert<'_, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: RenderInsertRows<Postgres>,
    Returning: RenderProjectable<Postgres>,
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
        render::write_insert::<S, Postgres, _, _>(
            &PostgresDialect,
            &self.columns,
            &self.returning,
            self.conflict.as_ref(),
            writer,
        )
    }

    /// Write bind parameters into a caller-provided native param vector.
    pub fn write_params(&self, params: &mut Vec<PostgresParam>) -> Result<(), PostgresError> {
        render::write_insert_params::<S, Postgres, _, _>(
            &PostgresDialect,
            &self.columns,
            &self.returning,
            self.conflict.as_ref(),
            params,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Result<Vec<PostgresParam>, PostgresError> {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}

impl<S, Shape, Filters, Returning, Conn> PostgresDelete<'_, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: RenderPredicateNodes<Postgres>,
    Returning: RenderProjectable<Postgres>,
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
        render::write_delete::<S, Postgres, _, _>(
            &PostgresDialect,
            self.alias,
            &self.filters,
            &self.returning,
            writer,
        )
    }

    /// Write bind parameters into a caller-provided native param vector.
    pub fn write_params(&self, params: &mut Vec<PostgresParam>) -> Result<(), PostgresError> {
        render::write_delete_params::<S, Postgres, _, _>(
            &PostgresDialect,
            self.alias,
            &self.filters,
            &self.returning,
            params,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Result<Vec<PostgresParam>, PostgresError> {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}

impl<S, Shape, Columns, Filters, Returning, Conn>
    PostgresUpdate<'_, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: RenderUpdateAssignments<Postgres>,
    Filters: RenderPredicateNodes<Postgres>,
    Returning: RenderProjectable<Postgres>,
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
        render::write_update::<S, Postgres, _, _, _>(
            &PostgresDialect,
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            writer,
        )
    }

    /// Write bind parameters into a caller-provided native param vector.
    pub fn write_params(&self, params: &mut Vec<PostgresParam>) -> Result<(), PostgresError> {
        render::write_update_params::<S, Postgres, _, _, _>(
            &PostgresDialect,
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            params,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Result<Vec<PostgresParam>, PostgresError> {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_encode_to_expected_param() {
        assert!(matches!(encode_to_param(&7i8), Ok(PostgresParam::Int16(7))));
        assert!(matches!(
            encode_to_param(&7i16),
            Ok(PostgresParam::Int16(7))
        ));
        assert!(matches!(
            encode_to_param(&7i32),
            Ok(PostgresParam::Int32(7))
        ));
        assert!(matches!(
            encode_to_param(&7i64),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            encode_to_param(&7isize),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            encode_to_param(&7i128),
            Ok(PostgresParam::Numeric(PostgresNumericInteger {
                negative: false,
                magnitude: 7
            }))
        ));
        assert!(matches!(encode_to_param(&7u8), Ok(PostgresParam::Int32(7))));
        assert!(matches!(
            encode_to_param(&7u16),
            Ok(PostgresParam::Int32(7))
        ));
        assert!(matches!(
            encode_to_param(&7u32),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            encode_to_param(&7usize),
            Ok(PostgresParam::Int64(7))
        ));
        assert!(matches!(
            encode_to_param(&7u64),
            Ok(PostgresParam::Numeric(PostgresNumericInteger {
                negative: false,
                magnitude: 7
            }))
        ));
        assert!(matches!(
            encode_to_param(&1.5f32),
            Ok(PostgresParam::Float32(value)) if value == 1.5
        ));
        assert!(matches!(
            encode_to_param(&1.5f64),
            Ok(PostgresParam::Float64(value)) if value == 1.5
        ));
    }

    #[test]
    fn text_bool_and_null_encode_through() {
        assert!(matches!(
            encode_to_param(&String::from("Ada")),
            Ok(PostgresParam::Text(value)) if value == "Ada"
        ));
        assert!(matches!(
            encode_to_param(&true),
            Ok(PostgresParam::Bool(true))
        ));
        assert!(matches!(
            encode_to_param(&Option::<i32>::None),
            Ok(PostgresParam::Null(_))
        ));
        assert!(matches!(
            encode_to_param(&Some(5i32)),
            Ok(PostgresParam::Int32(5))
        ));
    }

    #[test]
    fn bytes_encode_to_bytea_param() {
        assert!(matches!(
            encode_to_param(&vec![0xDEu8, 0xAD, 0xBE, 0xEF]),
            Ok(PostgresParam::Bytes(value)) if value == [0xDE, 0xAD, 0xBE, 0xEF]
        ));
        // The nullable form routes through the same Bytes param / a typed NULL.
        assert!(matches!(
            encode_to_param(&Some(vec![1u8, 2, 3])),
            Ok(PostgresParam::Bytes(value)) if value == [1, 2, 3]
        ));
        assert!(matches!(
            encode_to_param(&Option::<Vec<u8>>::None),
            Ok(PostgresParam::Null(_))
        ));
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn uuid_encodes_to_native_param() {
        let id = uuid::Uuid::from_u128(0x1234_5678_1234_5678_1234_5678_1234_5678);
        // Bare uuid::Uuid maps to a `uuid` column without a db_type override...
        assert_eq!(
            <uuid::Uuid as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Uuid
        );
        // ...encodes to a native uuid param...
        assert!(matches!(
            encode_to_param(&id),
            Ok(PostgresParam::Uuid(value)) if value == id
        ));
        // ...and so does a transparent #[derive(ColumnType)] newtype over it.
        #[derive(Clone, Debug, PartialEq, Eq, squealy::ColumnType)]
        #[column_type(db_type = "uuid")]
        struct UserId(uuid::Uuid);

        assert_eq!(
            <UserId as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Uuid
        );
        assert!(matches!(
            encode_to_param(&UserId(id)),
            Ok(PostgresParam::Uuid(value)) if value == id
        ));

        // A nullable `uuid::Uuid` column (or a left-joined UUID table) makes the table derive emit a
        // `uuid::Uuid: DecodeNullable<Postgres>` bound. This compiles only when that impl exists, so
        // it guards against the regression where bare-uuid metadata generated but failed on use.
        fn _assert_uuid_decode_nullable<T: squealy::DecodeNullable<Postgres>>() {}
        _assert_uuid_decode_nullable::<uuid::Uuid>();
    }

    #[cfg(feature = "systemtime")]
    #[test]
    fn system_time_encodes_to_native_timestamp_param() {
        let ts = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(
            <std::time::SystemTime as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Timestamp { tz: true }
        );
        assert!(matches!(
            encode_to_param(&ts),
            Ok(PostgresParam::SystemTime(value)) if value == ts
        ));
        // A nullable timestamp column (`deleted_at`, `expires_at`) makes the table derive emit a
        // `DecodeNullable<Postgres>` bound; this compiles only when that impl exists.
        fn _assert_decode_nullable<T: squealy::DecodeNullable<Postgres>>() {}
        _assert_decode_nullable::<std::time::SystemTime>();
    }

    #[cfg(feature = "time")]
    #[test]
    fn time_offset_date_time_encodes_to_native_timestamp_param() {
        let ts = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(
            <time::OffsetDateTime as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Timestamp { tz: true }
        );
        assert!(matches!(
            encode_to_param(&ts),
            Ok(PostgresParam::TimeTimestamp(value)) if value == ts
        ));
        fn _assert_decode_nullable<T: squealy::DecodeNullable<Postgres>>() {}
        _assert_decode_nullable::<time::OffsetDateTime>();
    }

    #[cfg(feature = "chrono")]
    #[test]
    fn chrono_date_time_encodes_to_native_timestamp_param() {
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(
            <chrono::DateTime<chrono::Utc> as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Timestamp { tz: true }
        );
        assert!(matches!(
            encode_to_param(&ts),
            Ok(PostgresParam::ChronoTimestamp(value)) if value == ts
        ));
        fn _assert_decode_nullable<T: squealy::DecodeNullable<Postgres>>() {}
        _assert_decode_nullable::<chrono::DateTime<chrono::Utc>>();
    }

    #[cfg(feature = "serde")]
    #[test]
    fn json_encodes_to_jsonb_param() {
        let payload = serde_json::json!({ "ok": true, "n": 5 });
        assert!(matches!(
            encode_to_param(&Json(payload.clone())),
            Ok(PostgresParam::Json(value)) if value == payload
        ));
        assert_eq!(
            <Json<serde_json::Value> as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Jsonb
        );
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
}
