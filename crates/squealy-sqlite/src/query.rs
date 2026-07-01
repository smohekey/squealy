//! SQLite value codec and [`Backend`] impl.
//!
//! The value codec decodes result columns into Rust values and encodes bound parameters into the
//! driver-neutral [`SqliteValue`] (SQLite's five storage classes: NULL, INTEGER, REAL, TEXT, BLOB).
//! `SqliteValue` mirrors `rusqlite::types::Value`, so the execution slice (a later PR) bridges the two
//! trivially. This slice is driver-free: encoding is exercised by unit tests, and decoding reads from
//! an in-memory row of `SqliteValue`s.

use squealy::{Backend, Decode, Encode, ParamWriter, RowReader, Table};

use crate::{Sqlite, SqliteError};

/// SQLite's native value, one of the five storage classes. This is [`Backend::Param`] and the unit a
/// row is decoded from; it mirrors `rusqlite::types::Value`.
#[derive(Clone, Debug, PartialEq)]
pub enum SqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqliteValue {
    fn kind(&self) -> &'static str {
        match self {
            SqliteValue::Null => "NULL",
            SqliteValue::Integer(_) => "INTEGER",
            SqliteValue::Real(_) => "REAL",
            SqliteValue::Text(_) => "TEXT",
            SqliteValue::Blob(_) => "BLOB",
        }
    }
}

/// Reads columns positionally out of a decoded row (a slice of [`SqliteValue`]s) while a projected row
/// is decoded. Each [`read`](squealy::RowReader::read) consumes the next column, mirroring the order
/// the projection rendered them into the `SELECT` list.
pub struct SqliteRowReader<'row> {
    values: &'row [SqliteValue],
    index: usize,
}

impl<'row> SqliteRowReader<'row> {
    // Wired up by the executable query impls (a later slice), which decode each result row.
    #[allow(dead_code)]
    pub(crate) fn new(values: &'row [SqliteValue]) -> Self {
        Self { values, index: 0 }
    }

    /// Consumes and returns the next column value.
    fn take(&mut self) -> Result<&'row SqliteValue, SqliteError> {
        let value = self
            .values
            .get(self.index)
            .ok_or(SqliteError::MissingColumn(self.index))?;
        self.index += 1;
        Ok(value)
    }
}

impl RowReader for SqliteRowReader<'_> {
    type Backend = Sqlite;

    fn read<T>(&mut self) -> Result<T, SqliteError>
    where
        T: Decode<Sqlite>,
    {
        T::decode(self)
    }
}

/// A decode-time type mismatch (the column's storage class does not match the target type).
fn wrong_kind(target: &'static str, value: &SqliteValue) -> SqliteError {
    SqliteError::Decode {
        target,
        found: value.kind(),
    }
}

// --- Decode: SqliteValue -> Rust ---

impl Decode<Sqlite> for i64 {
    fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
        match row.take()? {
            SqliteValue::Integer(value) => Ok(*value),
            other => Err(wrong_kind("i64", other)),
        }
    }
}

macro_rules! impl_decode_from_i64 {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<Sqlite> for $ty {
            fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
                <$ty>::try_from(i64::decode(row)?)
                    .map_err(|_| SqliteError::Conversion(stringify!($ty)))
            }
        })*
    };
}
impl_decode_from_i64!(i8, i16, i32, isize, u8, u16, u32);

/// Wide integers (`u64`/`usize`/`i128`/`u128`) are stored as `INTEGER` when they fit `i64`, else as a
/// decimal `TEXT` (SQLite has no unsigned or 128-bit integer), so decoding accepts either.
macro_rules! impl_decode_wide_integer {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<Sqlite> for $ty {
            fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
                match row.take()? {
                    SqliteValue::Integer(value) => {
                        <$ty>::try_from(*value).map_err(|_| SqliteError::Conversion(stringify!($ty)))
                    }
                    SqliteValue::Text(text) => {
                        text.parse().map_err(|_| SqliteError::Conversion(stringify!($ty)))
                    }
                    other => Err(wrong_kind(stringify!($ty), other)),
                }
            }
        })*
    };
}
impl_decode_wide_integer!(u64, usize, i128, u128);

impl Decode<Sqlite> for f64 {
    fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
        match row.take()? {
            SqliteValue::Real(value) => Ok(*value),
            // A REAL column can come back as INTEGER when the value has no fractional part.
            SqliteValue::Integer(value) => Ok(*value as f64),
            other => Err(wrong_kind("f64", other)),
        }
    }
}

impl Decode<Sqlite> for f32 {
    fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
        Ok(f64::decode(row)? as f32)
    }
}

impl Decode<Sqlite> for bool {
    fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
        match row.take()? {
            SqliteValue::Integer(value) => Ok(*value != 0),
            other => Err(wrong_kind("bool", other)),
        }
    }
}

impl Decode<Sqlite> for String {
    fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
        match row.take()? {
            SqliteValue::Text(text) => Ok(text.clone()),
            other => Err(wrong_kind("String", other)),
        }
    }
}

impl Decode<Sqlite> for Vec<u8> {
    fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
        match row.take()? {
            SqliteValue::Blob(bytes) => Ok(bytes.clone()),
            other => Err(wrong_kind("Vec<u8>", other)),
        }
    }
}

impl<const N: usize> Decode<Sqlite> for [u8; N] {
    fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
        let bytes = <Vec<u8>>::decode(row)?;
        <[u8; N]>::try_from(bytes).map_err(|_| SqliteError::Conversion("fixed-size byte array"))
    }
}

impl<T> Decode<Sqlite> for Option<T>
where
    T: Decode<Sqlite>,
{
    fn decode(row: &mut SqliteRowReader<'_>) -> Result<Self, SqliteError> {
        // Peek the next column: a SQL NULL decodes to `None` (consuming the column); otherwise the
        // inner `Decode` reads it. Decoding through `Decode for T` (rather than a driver conversion)
        // lets nullable `ColumnType` newtype columns project.
        match row.values.get(row.index) {
            None => Err(SqliteError::MissingColumn(row.index)),
            Some(SqliteValue::Null) => {
                row.index += 1;
                Ok(None)
            }
            Some(_) => T::decode(row).map(Some),
        }
    }
}

/// Appends bound values as the native [`SqliteValue`]; each [`Encode<Sqlite>`] impl pushes exactly one.
#[doc(hidden)]
pub struct SqliteParamWriter<'param> {
    params: &'param mut Vec<SqliteValue>,
}

impl<'param> SqliteParamWriter<'param> {
    pub(crate) fn new(params: &'param mut Vec<SqliteValue>) -> Self {
        Self { params }
    }

    pub fn push(&mut self, value: SqliteValue) {
        self.params.push(value);
    }
}

impl ParamWriter for SqliteParamWriter<'_> {
    type Backend = Sqlite;

    fn write<T>(&mut self, value: &T) -> Result<(), SqliteError>
    where
        T: Encode<Sqlite>,
    {
        value.encode(self)
    }
}

/// Encodes a single value into one native [`SqliteValue`], asserting the one-literal-one-parameter
/// invariant the renderer relies on. Used by the codec unit tests.
#[cfg(test)]
pub(crate) fn encode_to_value<T>(value: &T) -> Result<SqliteValue, SqliteError>
where
    T: Encode<Sqlite>,
{
    let mut params = Vec::with_capacity(1);
    value.encode(&mut SqliteParamWriter::new(&mut params))?;
    let mut params = params.into_iter();
    let param = params
        .next()
        .ok_or(SqliteError::Conversion("bind produced no parameter"))?;
    if params.next().is_some() {
        return Err(SqliteError::Conversion(
            "bind produced more than one parameter",
        ));
    }
    Ok(param)
}

// --- Encode: Rust -> SqliteValue ---

macro_rules! impl_encode {
    ($($ty:ty => |$value:ident| $param:expr),* $(,)?) => {
        $(impl Encode<Sqlite> for $ty {
            fn encode(&self, out: &mut SqliteParamWriter<'_>) -> Result<(), SqliteError> {
                let $value = self;
                out.push($param);
                Ok(())
            }
        })*
    };
}

impl_encode! {
    i8 => |v| SqliteValue::Integer(i64::from(*v)),
    i16 => |v| SqliteValue::Integer(i64::from(*v)),
    i32 => |v| SqliteValue::Integer(i64::from(*v)),
    i64 => |v| SqliteValue::Integer(*v),
    isize => |v| SqliteValue::Integer(*v as i64),
    u8 => |v| SqliteValue::Integer(i64::from(*v)),
    u16 => |v| SqliteValue::Integer(i64::from(*v)),
    u32 => |v| SqliteValue::Integer(i64::from(*v)),
    f32 => |v| SqliteValue::Real(f64::from(*v)),
    f64 => |v| SqliteValue::Real(*v),
    bool => |v| SqliteValue::Integer(i64::from(*v)),
}

/// SQLite `INTEGER` is signed 64-bit, so a `u64`/`usize`/`i128`/`u128` that overflows `i64` is sent as
/// a decimal `TEXT` (which SQLite accepts and comparisons still order lexically only within the same
/// affinity — round-tripping is preserved by the matching decode). Narrower-fitting values are native
/// `INTEGER`.
macro_rules! impl_encode_wide_integer {
    ($($ty:ty),* $(,)?) => {
        $(impl Encode<Sqlite> for $ty {
            fn encode(&self, out: &mut SqliteParamWriter<'_>) -> Result<(), SqliteError> {
                out.push(match i64::try_from(*self) {
                    Ok(value) => SqliteValue::Integer(value),
                    Err(_) => SqliteValue::Text(self.to_string()),
                });
                Ok(())
            }
        })*
    };
}
impl_encode_wide_integer!(u64, usize, i128, u128);

impl Encode<Sqlite> for str {
    fn encode(&self, out: &mut SqliteParamWriter<'_>) -> Result<(), SqliteError> {
        out.push(SqliteValue::Text(self.to_owned()));
        Ok(())
    }
}

impl Encode<Sqlite> for String {
    fn encode(&self, out: &mut SqliteParamWriter<'_>) -> Result<(), SqliteError> {
        out.push(SqliteValue::Text(self.clone()));
        Ok(())
    }
}

impl Encode<Sqlite> for Vec<u8> {
    fn encode(&self, out: &mut SqliteParamWriter<'_>) -> Result<(), SqliteError> {
        out.push(SqliteValue::Blob(self.clone()));
        Ok(())
    }
}

impl<const N: usize> Encode<Sqlite> for [u8; N] {
    fn encode(&self, out: &mut SqliteParamWriter<'_>) -> Result<(), SqliteError> {
        out.push(SqliteValue::Blob(self.to_vec()));
        Ok(())
    }
}

impl<T> Encode<Sqlite> for Option<T>
where
    T: Encode<Sqlite>,
{
    fn encode(&self, out: &mut SqliteParamWriter<'_>) -> Result<(), SqliteError> {
        match self {
            Some(value) => value.encode(out),
            None => {
                out.push(SqliteValue::Null);
                Ok(())
            }
        }
    }
}

impl Backend for Sqlite {
    type Error = SqliteError;
    type RowReader<'row> = SqliteRowReader<'row>;
    type ParamWriter<'param> = SqliteParamWriter<'param>;
    type Param = SqliteValue;

    fn param_writer(params: &mut Vec<Self::Param>) -> Self::ParamWriter<'_> {
        SqliteParamWriter::new(params)
    }

    fn no_rows_error() -> Self::Error {
        SqliteError::NoRows
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
    use super::{SqliteRowReader, SqliteValue, encode_to_value};
    use squealy::RowReader;

    #[test]
    fn encodes_primitives() {
        assert_eq!(encode_to_value(&42i64).unwrap(), SqliteValue::Integer(42));
        assert_eq!(encode_to_value(&7i32).unwrap(), SqliteValue::Integer(7));
        assert_eq!(encode_to_value(&true).unwrap(), SqliteValue::Integer(1));
        assert_eq!(encode_to_value(&false).unwrap(), SqliteValue::Integer(0));
        assert_eq!(encode_to_value(&1.5f64).unwrap(), SqliteValue::Real(1.5));
        assert_eq!(
            encode_to_value(&String::from("hi")).unwrap(),
            SqliteValue::Text("hi".to_owned())
        );
        assert_eq!(
            encode_to_value(&vec![1u8, 2, 3]).unwrap(),
            SqliteValue::Blob(vec![1, 2, 3])
        );
        assert_eq!(
            encode_to_value(&Option::<i32>::None).unwrap(),
            SqliteValue::Null
        );
    }

    #[test]
    fn encodes_wide_integers_as_text_when_out_of_i64_range() {
        // Fits i64 -> INTEGER.
        assert_eq!(encode_to_value(&5u64).unwrap(), SqliteValue::Integer(5));
        // Beyond i64::MAX -> decimal TEXT.
        let big = u64::MAX;
        assert_eq!(
            encode_to_value(&big).unwrap(),
            SqliteValue::Text(big.to_string())
        );
    }

    #[test]
    fn decodes_primitives() {
        fn decode<T: squealy::Decode<crate::Sqlite>>(values: &[SqliteValue]) -> T {
            SqliteRowReader::new(values).read::<T>().unwrap()
        }
        assert_eq!(decode::<i64>(&[SqliteValue::Integer(42)]), 42);
        assert_eq!(decode::<i32>(&[SqliteValue::Integer(7)]), 7);
        assert!(decode::<bool>(&[SqliteValue::Integer(1)]));
        assert!(!decode::<bool>(&[SqliteValue::Integer(0)]));
        assert_eq!(decode::<f64>(&[SqliteValue::Real(1.5)]), 1.5);
        // A REAL target accepts an INTEGER column.
        assert_eq!(decode::<f64>(&[SqliteValue::Integer(3)]), 3.0);
        assert_eq!(
            decode::<String>(&[SqliteValue::Text("hi".to_owned())]),
            "hi"
        );
        assert_eq!(decode::<Vec<u8>>(&[SqliteValue::Blob(vec![9])]), vec![9]);
        assert_eq!(decode::<Option<i32>>(&[SqliteValue::Null]), None);
        assert_eq!(decode::<Option<i32>>(&[SqliteValue::Integer(4)]), Some(4));
    }

    #[test]
    fn decodes_wide_integer_from_text() {
        fn decode<T: squealy::Decode<crate::Sqlite>>(values: &[SqliteValue]) -> T {
            SqliteRowReader::new(values).read::<T>().unwrap()
        }
        let big = u64::MAX;
        assert_eq!(decode::<u64>(&[SqliteValue::Text(big.to_string())]), big);
        assert_eq!(decode::<u64>(&[SqliteValue::Integer(5)]), 5u64);
    }

    #[test]
    fn decode_rejects_wrong_storage_class() {
        let values = [SqliteValue::Text("x".to_owned())];
        let mut reader = SqliteRowReader::new(&values);
        assert!(reader.read::<i64>().is_err());
    }
}
