//! MySQL query runtime.
//!
//! This module is the query-execution counterpart to the schema management in [`crate`]: the value
//! codec (decoding result columns into Rust values and encoding bound parameters into the driver's
//! value type) and the [`Backend`] impl. The executable query objects build on top of it.

use mysql_async::Value;
use mysql_async::prelude::FromValue;
use squealy::{Backend, BindValue, BindValueKind, Decode, FloatWidth, Table};

use crate::{Mysql, MysqlError};

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
