//! MySQL query runtime.
//!
//! This module is the query-execution counterpart to the schema management in [`crate`]: the value
//! codec (decoding result columns into Rust values and encoding bound parameters into the driver's
//! value type) and the [`Backend`] impl. The executable query objects build on top of it.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use mysql_async::Value;
use mysql_async::prelude::{FromValue, Queryable};
use squealy::{
    Backend, Connection, Decode, DeleteQuery, Encode, ExecutableDeleteQuery, ExecutableInsertQuery,
    ExecutableSelectQuery, ExecutableUpdateQuery, HNil, InsertQuery, InsertRows, InsertableTable,
    NoRuntimeParams, ParamWriter, PredicateNodes, PreparedParamValues, Projectable,
    ProjectionShape, QueryBuilder, RenderInsertRows, RenderPredicateNodes, RenderProjectable,
    RenderSelectAst, RenderUpdateAssignments, RowsAffected, SelectAst, SelectQuery, Selected,
    SourceAlias, Table, TableProjection, UpdateAssignments, UpdateQuery, UpdateableTable, render,
};

use crate::{Mysql, MysqlConnection, MysqlError};

/// Reads columns positionally out of a [`mysql_async::Row`] while a projected row is decoded.
///
/// Each [`read`](squealy::RowReader::read) consumes the next column, mirroring the order the
/// projection rendered them into the `SELECT` list.
pub struct MysqlRowReader<'row> {
    row: &'row mut mysql_async::Row,
    index: usize,
}

impl<'row> MysqlRowReader<'row> {
    // Wired up by the executable query impls (the next step), which decode each result row.
    #[allow(dead_code)]
    pub(crate) fn new(row: &'row mut mysql_async::Row) -> Self {
        Self { row, index: 0 }
    }

    fn take<T>(&mut self) -> Result<T, MysqlError>
    where
        T: FromValue,
    {
        let column = self.index;
        let value = self
            .row
            .take_opt::<T, usize>(column)
            .ok_or(MysqlError::MissingColumn(column))?
            .map_err(|source| MysqlError::Decode { column, source })?;
        self.index += 1;
        Ok(value)
    }

    /// Decode a possibly-`NULL` value by peeking the current column without consuming it: a SQL
    /// `NULL` yields `None` (and skips the column), otherwise the inner [`Decode`] reads it. Decoding
    /// through `Decode for T` (rather than `Option<T>: FromValue`) lets nullable `ColumnType` newtype
    /// columns project, since those carry `Decode` but not `FromValue`.
    fn take_nullable<T>(&mut self) -> Result<Option<T>, MysqlError>
    where
        T: Decode<Mysql>,
    {
        let column = self.index;
        // Peek the current column, releasing the borrow before `T::decode` takes `&mut self`.
        match self.row.as_ref(column) {
            None => return Err(MysqlError::MissingColumn(column)),
            Some(Value::NULL) => {
                self.index += 1;
                return Ok(None);
            }
            Some(_) => {}
        }
        T::decode(self).map(Some)
    }
}

impl squealy::RowReader for MysqlRowReader<'_> {
    type Backend = Mysql;

    fn read<T>(&mut self) -> Result<T, MysqlError>
    where
        T: Decode<Mysql>,
    {
        T::decode(self)
    }
}

macro_rules! impl_mysql_decode_direct {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<Mysql> for $ty {
            fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
                row.take()
            }
        })*
    };
}

macro_rules! impl_mysql_decode_from_i64 {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<Mysql> for $ty {
            fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
                let value = row.take::<i64>()?;
                <$ty>::try_from(value).map_err(|_| MysqlError::Conversion(stringify!($ty)))
            }
        })*
    };
}

macro_rules! impl_mysql_decode_from_u64 {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<Mysql> for $ty {
            fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
                let value = row.take::<u64>()?;
                <$ty>::try_from(value).map_err(|_| MysqlError::Conversion(stringify!($ty)))
            }
        })*
    };
}

// `mysql_async` implements `FromValue` for these natively. `i128`/`u128` parse from MySQL's
// `DECIMAL`/text representation too, so a widened `SUM` (cast to `DECIMAL(65, 0)`) that exceeds 64
// bits still decodes — unlike routing through `i64`/`u64`, which would truncate or fail conversion.
impl_mysql_decode_direct!(
    i8,
    i16,
    i32,
    i64,
    i128,
    u8,
    u16,
    u32,
    u64,
    u128,
    f32,
    f64,
    bool,
    String,
    Vec<u8>
);
// Pointer-sized widths MySQL has no native column type for; the model carries them as 64-bit.
impl_mysql_decode_from_i64!(isize);
impl_mysql_decode_from_u64!(usize);

impl<T> Decode<Mysql> for Option<T>
where
    T: Decode<Mysql>,
{
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        row.take_nullable()
    }
}

/// Encode-side mirror of [`MysqlRowReader`]: appends bound values as the driver's [`Value`].
///
/// MySQL's native param representation is [`mysql_async::Value`] ([`Backend::Param`]); each
/// [`Encode<Mysql>`] impl pushes exactly one.
#[doc(hidden)]
pub struct MysqlParamWriter<'param> {
    params: &'param mut Vec<Value>,
}

impl<'param> MysqlParamWriter<'param> {
    pub(crate) fn new(params: &'param mut Vec<Value>) -> Self {
        Self { params }
    }

    pub fn push(&mut self, value: Value) {
        self.params.push(value);
    }
}

impl ParamWriter for MysqlParamWriter<'_> {
    type Backend = Mysql;

    fn write<T>(&mut self, value: &T) -> Result<(), MysqlError>
    where
        T: Encode<Mysql>,
    {
        value.encode(self)
    }
}

/// Encodes a single value into one native [`Value`] via [`Encode`], asserting the
/// one-literal-one-parameter invariant the renderer relies on. Used by the codec unit tests.
#[cfg(test)]
pub(crate) fn encode_to_value<T>(value: &T) -> Result<Value, MysqlError>
where
    T: Encode<Mysql>,
{
    let mut params = Vec::with_capacity(1);
    value.encode(&mut MysqlParamWriter::new(&mut params))?;
    let mut params = params.into_iter();
    let param = params
        .next()
        .ok_or(MysqlError::Conversion("bind produced no parameter"))?;
    if params.next().is_some() {
        return Err(MysqlError::Conversion(
            "bind produced more than one parameter",
        ));
    }
    Ok(param)
}

macro_rules! impl_mysql_encode {
    ($($ty:ty => |$value:ident| $param:expr),* $(,)?) => {
        $(impl Encode<Mysql> for $ty {
            fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
                let $value = self;
                out.push($param);
                Ok(())
            }
        })*
    };
}

// MySQL has no 128-bit integer type, so an `i128`/`u128` that overflows `i64`/`u64` is sent as a
// decimal string (`Value::Bytes`), which MySQL accepts for `DECIMAL`/`BIGINT` columns. The narrower
// widths always fit and become native `Int`/`UInt`.
impl_mysql_encode! {
    i8 => |v| Value::Int(i64::from(*v)),
    i16 => |v| Value::Int(i64::from(*v)),
    i32 => |v| Value::Int(i64::from(*v)),
    i64 => |v| Value::Int(*v),
    isize => |v| Value::Int(*v as i64),
    u8 => |v| Value::UInt(u64::from(*v)),
    u16 => |v| Value::UInt(u64::from(*v)),
    u32 => |v| Value::UInt(u64::from(*v)),
    u64 => |v| Value::UInt(*v),
    usize => |v| Value::UInt(*v as u64),
    f32 => |v| Value::Float(*v),
    f64 => |v| Value::Double(*v),
    bool => |v| Value::Int((*v).into()),
}

impl Encode<Mysql> for i128 {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        out.push(match i64::try_from(*self) {
            Ok(value) => Value::Int(value),
            Err(_) => Value::Bytes(self.to_string().into_bytes()),
        });
        Ok(())
    }
}

impl Encode<Mysql> for u128 {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        out.push(match u64::try_from(*self) {
            Ok(value) => Value::UInt(value),
            Err(_) => Value::Bytes(self.to_string().into_bytes()),
        });
        Ok(())
    }
}

impl Encode<Mysql> for str {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        out.push(Value::Bytes(self.as_bytes().to_vec()));
        Ok(())
    }
}

impl Encode<Mysql> for String {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        out.push(Value::Bytes(self.clone().into_bytes()));
        Ok(())
    }
}

impl Encode<Mysql> for Vec<u8> {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        out.push(Value::Bytes(self.clone()));
        Ok(())
    }
}

// Fixed-size byte arrays bind as a `BINARY(N)` value; decode errors unless exactly `N` bytes.
impl<const N: usize> Encode<Mysql> for [u8; N] {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        out.push(Value::Bytes(self.to_vec()));
        Ok(())
    }
}

impl<const N: usize> Decode<Mysql> for [u8; N] {
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        let bytes = row.take::<Vec<u8>>()?;
        <[u8; N]>::try_from(bytes).map_err(|_| MysqlError::Conversion("fixed-size byte array"))
    }
}

// `bytes::Bytes` binds to a `BLOB` column via `Vec<u8>` (behind the opt-in `bytes` feature).
#[cfg(feature = "bytes")]
impl Encode<Mysql> for bytes::Bytes {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        out.push(Value::Bytes(self.to_vec()));
        Ok(())
    }
}

#[cfg(feature = "bytes")]
impl Decode<Mysql> for bytes::Bytes {
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        Ok(bytes::Bytes::from(row.take::<Vec<u8>>()?))
    }
}

impl<T> Encode<Mysql> for Option<T>
where
    T: Encode<Mysql>,
{
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        match self {
            Some(value) => value.encode(out),
            None => {
                out.push(Value::NULL);
                Ok(())
            }
        }
    }
}

impl Backend for Mysql {
    type Error = MysqlError;

    type RowReader<'row> = MysqlRowReader<'row>;

    type ParamWriter<'param> = MysqlParamWriter<'param>;

    type Param = Value;

    fn param_writer(params: &mut Vec<Self::Param>) -> Self::ParamWriter<'_> {
        MysqlParamWriter::new(params)
    }

    fn no_rows_error() -> Self::Error {
        MysqlError::NoRows
    }

    fn write_table(
        &self,
        table: &(dyn Table + Sync),
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        crate::sql::write_table(table, writer)
    }
}

type BufferedRows = (Vec<mysql_async::Row>, u64);

/// Runs rendered SQL against a connection. Implemented for [`MysqlConnection`] (and, later,
/// transactions) so the query objects can be generic over where they execute. Mirrors the role of
/// PostgreSQL's executor trait; the `mysql_async` `&mut Conn` requirement is hidden behind the
/// connection's `Mutex` (see [`MysqlConnection::lock`]).
pub trait MysqlExecutor: Connection<Backend = Mysql> {
    /// Runs `sql` with `params`, buffering all result rows and the affected-row count.
    fn run_query<'query>(
        &'query self,
        sql: String,
        params: Vec<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<BufferedRows, MysqlError>> + Send + 'query>>;

    /// Runs `sql` with `params` for its effect only, returning the affected-row count.
    fn run_execute<'query>(
        &'query self,
        sql: String,
        params: Vec<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, MysqlError>> + Send + 'query>>;
}

impl MysqlExecutor for MysqlConnection {
    fn run_query<'query>(
        &'query self,
        sql: String,
        params: Vec<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<BufferedRows, MysqlError>> + Send + 'query>> {
        Box::pin(async move {
            let mut guard = self.lock().await;
            let mut result = guard
                .exec_iter(sql, mysql_async::Params::Positional(params))
                .await
                .map_err(MysqlError::Query)?;
            let rows = result
                .collect::<mysql_async::Row>()
                .await
                .map_err(MysqlError::Query)?;
            let affected = result.affected_rows();
            Ok((rows, affected))
        })
    }

    fn run_execute<'query>(
        &'query self,
        sql: String,
        params: Vec<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<u64, MysqlError>> + Send + 'query>> {
        Box::pin(async move {
            let mut guard = self.lock().await;
            guard
                .exec_drop(sql, mysql_async::Params::Positional(params))
                .await
                .map_err(MysqlError::Query)?;
            Ok(guard.affected_rows())
        })
    }
}

/// Executes a rendered statement and yields its result rows, decoded.
///
/// A MySQL connection serves one statement at a time and `mysql_async`'s result borrows the
/// connection, so rather than hold the `Mutex` guard across a lazy stream, the rows are collected up
/// front (while the guard is held) and then decoded one at a time from the buffer. This also carries
/// the affected-row count, so the same type backs both selects and mutations.
pub struct MysqlRows<'query, Row, Conn = MysqlConnection> {
    state: MysqlRowsState<'query>,
    affected_rows: Option<u64>,
    _row: PhantomData<Row>,
    _connection: PhantomData<fn() -> Conn>,
}

enum MysqlRowsState<'query> {
    Pending(Pin<Box<dyn Future<Output = Result<BufferedRows, MysqlError>> + Send + 'query>>),
    Rows(std::vec::IntoIter<mysql_async::Row>),
    Done,
}

impl<'query, Row, Conn> MysqlRows<'query, Row, Conn>
where
    Conn: MysqlExecutor,
{
    fn query(connection: &'query Conn, sql: String, params: Vec<Value>) -> Self {
        Self {
            state: MysqlRowsState::Pending(connection.run_query(sql, params)),
            affected_rows: None,
            _row: PhantomData,
            _connection: PhantomData,
        }
    }

    fn error(error: MysqlError) -> Self {
        Self {
            state: MysqlRowsState::Pending(Box::pin(std::future::ready(Err(error)))),
            affected_rows: None,
            _row: PhantomData,
            _connection: PhantomData,
        }
    }
}

impl<Row, Conn> Stream for MysqlRows<'_, Row, Conn>
where
    Row: Decode<Mysql>,
{
    type Item = Result<Row, MysqlError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                MysqlRowsState::Pending(future) => match future.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok((rows, affected))) => {
                        this.affected_rows = Some(affected);
                        this.state = MysqlRowsState::Rows(rows.into_iter());
                    }
                    Poll::Ready(Err(error)) => {
                        this.state = MysqlRowsState::Done;
                        return Poll::Ready(Some(Err(error)));
                    }
                },
                MysqlRowsState::Rows(iter) => match iter.next() {
                    Some(mut row) => {
                        let mut reader = MysqlRowReader::new(&mut row);
                        return Poll::Ready(Some(Row::decode(&mut reader)));
                    }
                    None => {
                        this.state = MysqlRowsState::Done;
                        return Poll::Ready(None);
                    }
                },
                MysqlRowsState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl<Row, Conn> Unpin for MysqlRows<'_, Row, Conn> {}

impl<Row, Conn> RowsAffected for MysqlRows<'_, Row, Conn> {
    fn rows_affected(&self) -> Option<u64> {
        self.affected_rows
    }
}

fn collect_mysql_params(
    capacity: usize,
    write: impl FnOnce(&mut Vec<Value>) -> Result<(), MysqlError>,
) -> Result<Vec<Value>, MysqlError> {
    let mut params = Vec::with_capacity(capacity);
    write(&mut params)?;
    Ok(params)
}

/// Resolves a prepared statement's recorded bindings into driver values: static literals are
/// reused as-is (already encoded), runtime slots are encoded from the supplied parameter values.
#[allow(dead_code)]
fn resolve_prepared_params<Shape, Params>(
    bindings: &[render::SqlParam<Mysql>],
    params: &Params,
) -> Result<Vec<Value>, MysqlError>
where
    Shape: squealy::HList,
    Params: PreparedParamValues<Shape, Mysql>,
{
    let mut values = Vec::with_capacity(bindings.len());
    for binding in bindings {
        match binding {
            render::SqlParam::Static(value) => values.push(value.clone()),
            render::SqlParam::Runtime(index) => {
                let mut writer = MysqlParamWriter::new(&mut values);
                if !params.write_param_at(*index, &mut writer)? {
                    return Err(MysqlError::Conversion("prepared parameter"));
                }
            }
        }
    }
    Ok(values)
}

/// Renders SQL into a freshly allocated string (the renderer only ever emits UTF-8).
fn rendered_sql(write: impl FnOnce(&mut Vec<u8>) -> std::io::Result<()>) -> String {
    let mut buffer = Vec::new();
    write(&mut buffer).expect("render SQL");
    String::from_utf8(buffer).expect("renderer emits UTF-8")
}

fn execute_error<'query>(
    error: MysqlError,
) -> Pin<Box<dyn Future<Output = Result<u64, MysqlError>> + Send + 'query>> {
    Box::pin(std::future::ready(Err(error)))
}

include!("query_objects.rs");

#[cfg(test)]
mod tests {
    use super::encode_to_value;
    use mysql_async::Value;

    #[test]
    fn encodes_in_range_integers_natively() {
        assert_eq!(encode_to_value(&42i64).unwrap(), Value::Int(42));
        assert_eq!(encode_to_value(&42u64).unwrap(), Value::UInt(42));
        assert_eq!(encode_to_value(&true).unwrap(), Value::Int(1));
    }

    #[test]
    fn encodes_out_of_range_128_bit_as_a_decimal_string() {
        // MySQL has no 128-bit integer, so a value beyond i64/u64 is sent as a decimal string.
        let big = i128::from(i64::MAX) + 1;
        assert_eq!(
            encode_to_value(&big).unwrap(),
            Value::Bytes(big.to_string().into_bytes())
        );
    }

    #[test]
    fn decodes_out_of_range_128_bit_from_a_decimal_string() {
        use mysql_async::prelude::FromValue;
        // A widened `SUM(BIGINT UNSIGNED)` above 64 bits comes back as a `DECIMAL` (text); `i128` /
        // `u128` must parse it rather than truncating through `i64` / `u64`.
        let big = i128::from(i64::MAX) + 1;
        assert_eq!(
            i128::from_value(Value::Bytes(big.to_string().into_bytes())),
            big
        );

        let big_u = u128::from(u64::MAX) + 1;
        assert_eq!(
            u128::from_value(Value::Bytes(big_u.to_string().into_bytes())),
            big_u
        );
    }

    #[test]
    fn encodes_text_and_null() {
        assert_eq!(
            encode_to_value(&String::from("hi")).unwrap(),
            Value::Bytes(b"hi".to_vec())
        );
        assert_eq!(encode_to_value(&Option::<i32>::None).unwrap(), Value::NULL);
    }

    #[test]
    fn encodes_bytes_as_value_bytes() {
        assert_eq!(
            encode_to_value(&vec![0xDEu8, 0xAD, 0xBE, 0xEF]).unwrap(),
            Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])
        );
        assert_eq!(
            encode_to_value(&Some(vec![1u8, 2, 3])).unwrap(),
            Value::Bytes(vec![1, 2, 3])
        );
        assert_eq!(
            encode_to_value(&Option::<Vec<u8>>::None).unwrap(),
            Value::NULL
        );
    }
}
