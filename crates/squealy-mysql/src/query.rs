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
use squealy::{Backend, BindValue, BindValueKind, Decode, FloatWidth, RowsAffected, Table};

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

// `mysql_async` implements `FromValue` for these natively.
impl_mysql_decode_direct!(i8, i16, i32, i64, u8, u16, u32, u64, f32, f64, bool, String);
// Widths MySQL stores as 64-bit but the model carries as wider/pointer-sized types.
impl_mysql_decode_from_i64!(isize, i128);
impl_mysql_decode_from_u64!(usize, u128);

impl<T> Decode<Mysql> for Option<T>
where
    T: FromValue,
{
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        // `FromValue for Option<T>` maps SQL `NULL` to `None`.
        row.take()
    }
}

/// Encodes a neutral [`BindValue`] into the driver's [`Value`].
///
/// MySQL has no 128-bit integer type, so an `i128`/`u128` that overflows `i64`/`u64` is sent as a
/// decimal string (`Value::Bytes`), which MySQL accepts for `DECIMAL`/`BIGINT` columns. The narrower
/// widths always fit and become native `Int`/`UInt`.
// Wired up by the executable query impls (the next step), which bind parameters into the statement.
#[allow(dead_code)]
pub(crate) fn bind_value(value: BindValue) -> Value {
    match value.into_kind() {
        BindValueKind::Int { value, width: _ } => match i64::try_from(value) {
            Ok(value) => Value::Int(value),
            Err(_) => Value::Bytes(value.to_string().into_bytes()),
        },
        BindValueKind::UInt { value, width: _ } => match u64::try_from(value) {
            Ok(value) => Value::UInt(value),
            Err(_) => Value::Bytes(value.to_string().into_bytes()),
        },
        BindValueKind::Float { value, width } => match width {
            FloatWidth::F32 => Value::Float(value as f32),
            FloatWidth::F64 => Value::Double(value),
        },
        BindValueKind::Text(text) => Value::Bytes(text.into_bytes()),
        BindValueKind::Bool(value) => Value::Int(value.into()),
        BindValueKind::Null => Value::NULL,
    }
}

impl Backend for Mysql {
    type Error = MysqlError;

    type RowReader<'row> = MysqlRowReader<'row>;

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

/// Executes a rendered statement and yields its result rows, decoded.
///
/// A MySQL connection serves one statement at a time and `mysql_async`'s result borrows the
/// connection, so rather than hold the `Mutex` guard across a lazy stream, the rows are collected up
/// front (while the guard is held) and then decoded one at a time from the buffer. This also carries
/// the affected-row count, so the same type backs both selects and mutations.
pub struct MysqlRows<'query, Row> {
    state: MysqlRowsState<'query>,
    affected_rows: Option<u64>,
    _row: PhantomData<Row>,
}

type BufferedRows = (Vec<mysql_async::Row>, u64);

enum MysqlRowsState<'query> {
    Pending(Pin<Box<dyn Future<Output = Result<BufferedRows, MysqlError>> + Send + 'query>>),
    Rows(std::vec::IntoIter<mysql_async::Row>),
    Done,
}

impl<'query, Row> MysqlRows<'query, Row> {
    #[allow(dead_code)] // Driven by the executable query impls (next).
    pub(crate) fn query(
        connection: &'query MysqlConnection,
        sql: String,
        params: Vec<Value>,
    ) -> Self {
        Self {
            state: MysqlRowsState::Pending(Box::pin(run_query(connection, sql, params))),
            affected_rows: None,
            _row: PhantomData,
        }
    }

    #[allow(dead_code)] // Driven by the executable query impls (next).
    pub(crate) fn error(error: MysqlError) -> Self {
        Self {
            state: MysqlRowsState::Pending(Box::pin(std::future::ready(Err(error)))),
            affected_rows: None,
            _row: PhantomData,
        }
    }
}

/// Locks the connection, runs `sql` with `params`, and buffers all result rows and the affected count.
async fn run_query(
    connection: &MysqlConnection,
    sql: String,
    params: Vec<Value>,
) -> Result<BufferedRows, MysqlError> {
    let mut guard = connection.lock().await;
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
}

impl<Row> Stream for MysqlRows<'_, Row>
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

impl<Row> Unpin for MysqlRows<'_, Row> {}

impl<Row> RowsAffected for MysqlRows<'_, Row> {
    fn rows_affected(&self) -> Option<u64> {
        self.affected_rows
    }
}

#[cfg(test)]
mod tests {
    use super::bind_value;
    use mysql_async::Value;
    use squealy::BindValue;

    #[test]
    fn encodes_in_range_integers_natively() {
        assert_eq!(bind_value(BindValue::Int(42)), Value::Int(42));
        assert_eq!(bind_value(BindValue::UInt(42)), Value::UInt(42));
        assert_eq!(bind_value(BindValue::Bool(true)), Value::Int(1));
    }

    #[test]
    fn encodes_out_of_range_128_bit_as_a_decimal_string() {
        // MySQL has no 128-bit integer, so a value beyond i64/u64 is sent as a decimal string.
        let big = i128::from(i64::MAX) + 1;
        assert_eq!(
            bind_value(BindValue::Int(big)),
            Value::Bytes(big.to_string().into_bytes())
        );
    }

    #[test]
    fn encodes_text_and_null() {
        assert_eq!(
            bind_value(BindValue::Text("hi".to_owned())),
            Value::Bytes(b"hi".to_vec())
        );
        assert_eq!(bind_value(BindValue::Null), Value::NULL);
    }
}
