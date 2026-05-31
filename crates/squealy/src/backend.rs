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

/// Backend-specific DDL generation.
pub trait Backend: Sized {
    type Error;

    type RowReader<'row>: RowReader<Backend = Self>;

    fn no_rows_error() -> Self::Error;

    /// Generate backend-specific SQL for a table.
    fn write_table(&self, table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()>;
}
