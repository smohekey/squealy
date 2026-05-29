use std::io::{self, Write};

use crate::TableSchema;

/// Generates backend-specific SQL from derived table schema metadata.
pub trait Generator {
    /// Generate backend-specific SQL for a table schema.
    fn write_table(&self, schema: &TableSchema, writer: &mut impl Write) -> io::Result<()>;
}
