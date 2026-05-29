use std::io::{self, Write};

use crate::Table;

/// Generates backend-specific SQL from derived table schema metadata.
pub trait Generator {
    /// Generate backend-specific SQL for a table schema.
    fn write_table<T: Table>(&self, writer: &mut impl Write) -> io::Result<()>;
}
