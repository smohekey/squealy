use std::io::{self, Write};

use crate::Table;

/// Generates backend-specific SQL from derived table metadata.
pub trait Generator {
    /// Generate backend-specific SQL for a table.
    fn write_table(&self, table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()>;
}
