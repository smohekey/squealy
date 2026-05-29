use std::io::{self, Write};

use crate::SchemaTable;

/// Generates backend-specific SQL from derived table metadata.
pub trait Generator {
    /// Generate backend-specific SQL for a table.
    fn write_table<T: SchemaTable>(&self, writer: &mut impl Write) -> io::Result<()>;
}
