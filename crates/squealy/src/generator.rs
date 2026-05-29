use std::io::{self, Write};

use crate::{ColumnSchema, ForeignKeySchema, IndexSchema, TableSchema};

/// Generates backend-specific SQL from derived table schema metadata.
pub trait Generator {
    /// Generate a full set of SQL statements for creating a table and its indexes.
    fn create_table(&self, schema: &TableSchema, writer: &mut impl Write) -> io::Result<()> {
        self.write_create_table_statement(schema, writer)?;
        for index in schema.indexes {
            writer.write_all(b"\n")?;
            self.write_create_index_statement(schema, index, writer)?;
        }
        Ok(())
    }

    /// Generate the backend-specific `CREATE TABLE` statement.
    fn write_create_table_statement(
        &self,
        schema: &TableSchema,
        writer: &mut impl Write,
    ) -> io::Result<()>;

    /// Generate the backend-specific `CREATE INDEX` statement for one index.
    fn write_create_index_statement(
        &self,
        table: &TableSchema,
        index: &IndexSchema,
        writer: &mut impl Write,
    ) -> io::Result<()>;

    /// Generate the backend-specific SQL fragment for one column definition.
    fn write_column_definition(
        &self,
        column: &ColumnSchema,
        writer: &mut impl Write,
    ) -> io::Result<()>;

    /// Generate the backend-specific SQL fragment for one foreign-key reference.
    fn write_foreign_key_reference(
        &self,
        reference: &ForeignKeySchema,
        writer: &mut impl Write,
    ) -> io::Result<()>;
}
