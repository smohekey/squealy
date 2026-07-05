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
    Backend, Connection, Decode, DeleteQuery, DeleteUsingQuery, Encode, ExecutableDeleteQuery,
    ExecutableDeleteUsingQuery, ExecutableInsertQuery, ExecutableSelectQuery,
    ExecutableSetSelectQuery, ExecutableUpdateFromQuery, ExecutableUpdateQuery, HNil, InsertQuery,
    InsertRows, InsertableTable, IntoInsertSelect, NoRuntimeParams, ParamWriter, PredicateNodes,
    PreparedParamValues, Projectable, ProjectionShape, QueryBuilder, RenderInsertRows,
    RenderPredicateNodes, RenderProjectable, RenderSelectAst, RenderUpdateAssignments,
    RowsAffected, SchemaTable, SelectAst, SelectQuery, Selected, SetArm, SetLeaf, SetOperand,
    SetOperations, SetSelectModifiers, SetTail, SourceAlias, Table, TableProjection,
    UpdateAssignments, UpdateFromQuery, UpdateQuery, UpdateableTable, render,
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

// ===== Optional type codecs (each behind its feature flag) =====

// `uuid::Uuid` binds to a `CHAR(36)` column as its hyphenated lowercase text form (MySQL has no
// native UUID type — the column renders `CHAR(36)`, see `sql::write_mysql_sql_type`).
#[cfg(feature = "uuid")]
impl Encode<Mysql> for uuid::Uuid {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        out.push(Value::Bytes(self.to_string().into_bytes()));
        Ok(())
    }
}

#[cfg(feature = "uuid")]
impl Decode<Mysql> for uuid::Uuid {
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        let text = row.take::<String>()?;
        uuid::Uuid::parse_str(&text).map_err(|_| MysqlError::Conversion("uuid::Uuid"))
    }
}

// serde-backed `Json<T>`: any `T: Serialize`/`DeserializeOwned` round-trips through a `JSON` column.
// The wrapper lives in this crate (mirroring the PostgreSQL backend's `Json<T>`); the column renders
// `JSON` (see `sql::write_mysql_sql_type`), and MySQL accepts/returns the JSON text.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Json<T>(pub T);

// Maps to `ColumnType::Json` (not `Jsonb`): MySQL has a single `JSON` type, which the introspector
// reads back as `SqlType::Json`, so the desired model settles against a published schema instead of
// churning a `Jsonb` vs `Json` type change. (The PostgreSQL backend uses `Jsonb` for its own wrapper,
// where `jsonb` is the distinct, preferred physical type.)
#[cfg(feature = "serde")]
impl<T> squealy::HasColumnType for Json<T> {
    const COLUMN_TYPE: squealy::ColumnType = squealy::ColumnType::Json;
}

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
impl<T> Encode<Mysql> for Json<T>
where
    T: serde::Serialize,
{
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        let json =
            serde_json::to_vec(&self.0).map_err(|_| MysqlError::Conversion("serialize json"))?;
        out.push(Value::Bytes(json));
        Ok(())
    }
}

#[cfg(feature = "serde")]
impl<T> Decode<Mysql> for Json<T>
where
    T: serde::de::DeserializeOwned,
{
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        let bytes = row.take::<Vec<u8>>()?;
        let inner = serde_json::from_slice(&bytes)
            .map_err(|_| MysqlError::Conversion("deserialize json"))?;
        Ok(Json(inner))
    }
}

// ===== UTC timestamp codecs (via the driver's `Value::Date`) =====
//
// `SystemTime` / `time::OffsetDateTime` / `chrono::DateTime<Utc>` all map to a `Timestamp { tz: true }`
// column (rendered bare `TIMESTAMP`). MySQL has no offset-carrying value, so each is converted to UTC
// and bound as a civil `Value::Date(year, month, day, hour, minute, second, microsecond)`; decoding
// reads the same shape back and reattaches UTC. Run the driver session with `time_zone = '+00:00'`
// (done in `Mysql::connect`) so MySQL stores/returns the `TIMESTAMP` without shifting it.
//
// Resolution is **microseconds**: the native datetime types map to a `TIMESTAMP(6)` column (see
// `HasColumnType`), so the encoders bind the full sub-second component. Against a lower-precision column
// (`db_type = "timestamp"`, fsp 0) MySQL rounds the value on store, which is the server's own behaviour.

// The civil components of a `Value::Date`: year, month, day, hour, minute, second, microsecond.
#[cfg(any(feature = "time", feature = "chrono", feature = "systemtime"))]
type DateParts = (u16, u8, u8, u8, u8, u8, u32);

// MySQL `TIMESTAMP` spans 1970-01-01 00:00:01 .. 2038-01-19 03:14:07 UTC (a 32-bit epoch). Binding an
// instant outside that range errors in strict mode and silently stores a zero date otherwise, so the
// encoders reject it up front with a clear error rather than corrupting the value.
#[cfg(any(feature = "time", feature = "chrono", feature = "systemtime"))]
fn check_timestamp_range(unix_seconds: i64) -> Result<(), MysqlError> {
    if (1..=2_147_483_647).contains(&unix_seconds) {
        Ok(())
    } else {
        Err(MysqlError::Conversion(
            "timestamp outside MySQL TIMESTAMP range (1970-01-01..2038-01-19 UTC)",
        ))
    }
}

// Reads a `Value::Date` from the next column, rejecting any other driver value and any out-of-range
// civil components. The range check matters for the `SystemTime` decoder, whose calendar math does not
// validate its inputs — a legacy/non-strict MySQL table can return a zero date (`0000-00-00 …`), which
// must fail rather than decode garbage. (The `time`/`chrono` decoders also reject these via their own
// constructors; validating here keeps all three consistent.)
#[cfg(any(feature = "time", feature = "chrono", feature = "systemtime"))]
fn take_date(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<DateParts, MysqlError> {
    match row.take::<Value>()? {
        Value::Date(year, month, day, hour, minute, second, micros)
            if (1..=12).contains(&month)
                && (1..=31).contains(&day)
                && hour <= 23
                && minute <= 59
                && second <= 59
                && micros <= 999_999 =>
        {
            Ok((year, month, day, hour, minute, second, micros))
        }
        _ => Err(MysqlError::Conversion("timestamp")),
    }
}

#[cfg(feature = "chrono")]
impl Encode<Mysql> for chrono::DateTime<chrono::Utc> {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        use chrono::{Datelike, Timelike};
        check_timestamp_range(self.timestamp())?;
        let year = u16::try_from(self.year())
            .map_err(|_| MysqlError::Conversion("chrono year out of range"))?;
        out.push(Value::Date(
            year,
            self.month() as u8,
            self.day() as u8,
            self.hour() as u8,
            self.minute() as u8,
            self.second() as u8,
            // Sub-second microseconds; clamp the leap-second range (`>= 1_000_000`) to the field max.
            self.timestamp_subsec_micros().min(999_999),
        ));
        Ok(())
    }
}

#[cfg(feature = "chrono")]
impl Decode<Mysql> for chrono::DateTime<chrono::Utc> {
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        let (year, month, day, hour, minute, second, micros) = take_date(row)?;
        let naive =
            chrono::NaiveDate::from_ymd_opt(i32::from(year), u32::from(month), u32::from(day))
                .and_then(|date| {
                    date.and_hms_micro_opt(
                        u32::from(hour),
                        u32::from(minute),
                        u32::from(second),
                        micros,
                    )
                })
                .ok_or(MysqlError::Conversion("chrono::DateTime"))?;
        Ok(naive.and_utc())
    }
}

#[cfg(feature = "time")]
impl Encode<Mysql> for time::OffsetDateTime {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        let utc = self.to_offset(time::UtcOffset::UTC);
        check_timestamp_range(utc.unix_timestamp())?;
        let year = u16::try_from(utc.year())
            .map_err(|_| MysqlError::Conversion("time year out of range"))?;
        out.push(Value::Date(
            year,
            u8::from(utc.month()),
            utc.day(),
            utc.hour(),
            utc.minute(),
            utc.second(),
            utc.microsecond(),
        ));
        Ok(())
    }
}

#[cfg(feature = "time")]
impl Decode<Mysql> for time::OffsetDateTime {
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        let (year, month, day, hour, minute, second, micros) = take_date(row)?;
        let month =
            time::Month::try_from(month).map_err(|_| MysqlError::Conversion("time month"))?;
        let date = time::Date::from_calendar_date(i32::from(year), month, day)
            .map_err(|_| MysqlError::Conversion("time::Date"))?;
        let clock = time::Time::from_hms_micro(hour, minute, second, micros)
            .map_err(|_| MysqlError::Conversion("time::Time"))?;
        Ok(time::PrimitiveDateTime::new(date, clock).assume_utc())
    }
}

#[cfg(feature = "systemtime")]
impl Encode<Mysql> for std::time::SystemTime {
    fn encode(&self, out: &mut MysqlParamWriter<'_>) -> Result<(), MysqlError> {
        // Microseconds from the Unix epoch, negative before it.
        let total_micros: i64 = match self.duration_since(std::time::UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_micros())
                .map_err(|_| MysqlError::Conversion("SystemTime too far in the future"))?,
            Err(before) => -i64::try_from(before.duration().as_micros())
                .map_err(|_| MysqlError::Conversion("SystemTime too far in the past"))?,
        };
        // Split into whole seconds (for the calendar math + range check) and the sub-second remainder;
        // `rem_euclid` keeps the microseconds non-negative for instants before the epoch.
        let seconds = total_micros.div_euclid(1_000_000);
        let micros = total_micros.rem_euclid(1_000_000) as u32;
        check_timestamp_range(seconds)?;
        let (year, month, day, hour, minute, second) = civil_from_unix_seconds(seconds);
        let year = u16::try_from(year)
            .map_err(|_| MysqlError::Conversion("SystemTime year out of range"))?;
        out.push(Value::Date(year, month, day, hour, minute, second, micros));
        Ok(())
    }
}

#[cfg(feature = "systemtime")]
impl Decode<Mysql> for std::time::SystemTime {
    fn decode(row: &mut <Mysql as Backend>::RowReader<'_>) -> Result<Self, MysqlError> {
        let (year, month, day, hour, minute, second, micros) = take_date(row)?;
        let seconds = unix_seconds_from_civil(i32::from(year), month, day, hour, minute, second);
        let total_micros = seconds * 1_000_000 + i64::from(micros);
        Ok(if total_micros >= 0 {
            std::time::UNIX_EPOCH + std::time::Duration::from_micros(total_micros as u64)
        } else {
            std::time::UNIX_EPOCH - std::time::Duration::from_micros((-total_micros) as u64)
        })
    }
}

// Civil-date conversions for `SystemTime`, which (unlike `time`/`chrono`) carries no calendar. These
// are Howard Hinnant's algorithms (http://howardhinnant.github.io/date_algorithms.html), valid for
// the proleptic Gregorian calendar across the full `Value::Date` year range with no dependencies.
#[cfg(feature = "systemtime")]
fn civil_from_unix_seconds(seconds: i64) -> (i64, u8, u8, u8, u8, u8) {
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (rem / 3_600) as u8,
        ((rem % 3_600) / 60) as u8,
        (rem % 60) as u8,
    )
}

#[cfg(feature = "systemtime")]
fn unix_seconds_from_civil(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> i64 {
    days_from_civil(i64::from(year), u32::from(month), u32::from(day)) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second)
}

#[cfg(feature = "systemtime")]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400; // [0, 399]
    let month_index = i64::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year = (153 * month_index + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(feature = "systemtime")]
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_position = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u8; // [1, 31]
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u8; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
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

    // A `uuid::Uuid` binds as its hyphenated `CHAR(36)` text, and the decode path (read the column as
    // a `String`, then `parse_str`) recovers it. Full row decoding is covered by integration tests.
    #[cfg(feature = "uuid")]
    #[test]
    fn uuid_encodes_as_char36_text_and_round_trips() {
        use mysql_async::prelude::FromValue;
        let id = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let encoded = encode_to_value(&id).unwrap();
        assert_eq!(encoded, Value::Bytes(id.to_string().into_bytes()));
        assert_eq!(id.to_string().len(), 36);
        let text = String::from_value(encoded);
        assert_eq!(uuid::Uuid::parse_str(&text).unwrap(), id);
        assert_eq!(
            <uuid::Uuid as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Uuid
        );
    }

    // `Json<T>` binds as its serialized JSON text (a `JSON` column), and the decode path
    // (`from_slice`) recovers the value.
    #[cfg(feature = "serde")]
    #[test]
    fn json_encodes_as_text_and_round_trips() {
        use mysql_async::prelude::FromValue;
        let payload = serde_json::json!({ "ok": true, "n": 5, "name": "Ada" });
        let encoded = encode_to_value(&super::Json(payload.clone())).unwrap();
        assert_eq!(encoded, Value::Bytes(serde_json::to_vec(&payload).unwrap()));
        let bytes = Vec::<u8>::from_value(encoded);
        let back: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, payload);
        assert_eq!(
            <super::Json<serde_json::Value> as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Json
        );
    }

    // 1_700_000_000 Unix seconds is 2023-11-14T22:13:20 UTC — a fixed anchor each timestamp codec must
    // bind as the same civil `Value::Date`, so the conversions are checked for real correctness rather
    // than mere self-consistency. The sub-second component is bound as microseconds (fsp 6).
    #[cfg(feature = "chrono")]
    #[test]
    fn chrono_encodes_as_utc_value_date() {
        let ts =
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 123_456_000).unwrap();
        assert_eq!(
            encode_to_value(&ts).unwrap(),
            Value::Date(2023, 11, 14, 22, 13, 20, 123_456)
        );
        assert_eq!(
            <chrono::DateTime<chrono::Utc> as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Timestamp {
                tz: true,
                precision: Some(6)
            }
        );
    }

    #[cfg(feature = "time")]
    #[test]
    fn time_encodes_as_utc_value_date_converting_the_offset() {
        let utc = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(
            encode_to_value(&utc).unwrap(),
            Value::Date(2023, 11, 14, 22, 13, 20, 0)
        );
        // A value in another offset is converted to UTC before binding (same instant, same civil date).
        let shifted = utc.to_offset(time::UtcOffset::from_hms(5, 30, 0).unwrap());
        assert_eq!(
            encode_to_value(&shifted).unwrap(),
            encode_to_value(&utc).unwrap()
        );
        assert_eq!(
            <time::OffsetDateTime as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Timestamp {
                tz: true,
                precision: Some(6)
            }
        );
    }

    #[cfg(feature = "systemtime")]
    #[test]
    fn systemtime_encodes_as_utc_value_date() {
        // The sub-second remainder is bound as microseconds (fsp 6).
        let ts = std::time::UNIX_EPOCH + std::time::Duration::from_micros(1_700_000_000_123_456);
        assert_eq!(
            encode_to_value(&ts).unwrap(),
            Value::Date(2023, 11, 14, 22, 13, 20, 123_456)
        );
        assert_eq!(
            <std::time::SystemTime as squealy::HasColumnType>::COLUMN_TYPE,
            squealy::ColumnType::Timestamp {
                tz: true,
                precision: Some(6)
            }
        );
    }

    // The dependency-free civil-date algorithm underpins the `SystemTime` codec, so check it against
    // fixed anchors (incl. the epoch and a leap day) and round-trip a spread of instants — including
    // pre-1970 — through both directions.
    #[cfg(feature = "systemtime")]
    #[test]
    fn civil_date_conversions_round_trip() {
        use super::{civil_from_unix_seconds, unix_seconds_from_civil};

        assert_eq!(civil_from_unix_seconds(0), (1970, 1, 1, 0, 0, 0));
        // 2000-02-29 (a leap day) at noon must survive the round trip.
        let leap = unix_seconds_from_civil(2000, 2, 29, 12, 0, 0);
        assert_eq!(civil_from_unix_seconds(leap), (2000, 2, 29, 12, 0, 0));

        for &seconds in &[
            0_i64,
            1,
            -1,
            86_399,
            -86_400,
            1_700_000_000,
            -2_208_988_800,  // 1900-01-01T00:00:00 UTC (well before the epoch)
            253_402_300_799, // 9999-12-31T23:59:59 UTC
        ] {
            let (year, month, day, hour, minute, second) = civil_from_unix_seconds(seconds);
            assert_eq!(
                unix_seconds_from_civil(year as i32, month, day, hour, minute, second),
                seconds,
                "round trip failed for {seconds}"
            );
        }
    }

    // Instants outside MySQL `TIMESTAMP`'s 1970..2038 UTC range are rejected at bind time rather than
    // erroring in strict mode / silently storing a zero date.
    #[cfg(any(feature = "chrono", feature = "time", feature = "systemtime"))]
    #[test]
    fn timestamps_outside_mysql_range_are_rejected() {
        #[cfg(feature = "chrono")]
        {
            let post_2038 =
                chrono::DateTime::<chrono::Utc>::from_timestamp(2_147_483_648, 0).unwrap();
            assert!(encode_to_value(&post_2038).is_err());
        }
        #[cfg(feature = "time")]
        {
            let post_2038 = time::OffsetDateTime::from_unix_timestamp(2_147_483_648).unwrap();
            assert!(encode_to_value(&post_2038).is_err());
        }
        #[cfg(feature = "systemtime")]
        {
            // Pre-1970, and the epoch second itself (TIMESTAMP's floor is 1970-01-01 00:00:01).
            let pre_1970 = std::time::UNIX_EPOCH - std::time::Duration::from_secs(1);
            assert!(encode_to_value(&pre_1970).is_err());
            assert!(encode_to_value(&std::time::UNIX_EPOCH).is_err());
        }
    }
}
