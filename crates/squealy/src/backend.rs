use std::io::{self, Write};

use crate::Table;

/// Backend-specific row cursor used while decoding a projected row.
pub trait RowReader: Sized {
    type Backend: Backend;

    fn read<T>(&mut self) -> Result<T, <Self::Backend as Backend>::Error>
    where
        T: Decode<Self::Backend>;
}

/// Decode a Rust value from a backend row reader.
pub trait Decode<B: Backend>: Sized {
    fn decode(row: &mut B::RowReader<'_>) -> Result<Self, B::Error>;
}

impl<B> Decode<B> for ()
where
    B: Backend,
{
    fn decode(_row: &mut B::RowReader<'_>) -> Result<Self, B::Error> {
        Ok(())
    }
}

/// Decode a nullable Rust value from a backend row reader.
///
/// Implementations return `None` when the next backend column is SQL `NULL`;
/// otherwise they decode and wrap the concrete value.
pub trait DecodeNullable<B: Backend>: Sized {
    fn decode_nullable(row: &mut B::RowReader<'_>) -> Result<Option<Self>, B::Error>;
}

macro_rules! impl_decode_nullable_via_option {
    ($($ty:ty),* $(,)?) => {
        $(impl<B> DecodeNullable<B> for $ty
        where
            B: Backend,
            Option<$ty>: Decode<B>,
        {
            fn decode_nullable(row: &mut B::RowReader<'_>) -> Result<Option<Self>, B::Error> {
                row.read::<Option<$ty>>()
            }
        })*
    };
}

impl_decode_nullable_via_option! {
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    f32,
    f64,
    String,
    bool,
}

/// Backend-specific DDL generation.
pub trait Backend: Sized {
    type Error;

    type RowReader<'row>: RowReader<Backend = Self>;

    fn no_rows_error() -> Self::Error;

    /// Generate backend-specific SQL for a table.
    fn write_table(&self, table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()>;
}
