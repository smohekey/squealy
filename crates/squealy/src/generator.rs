use crate::{ColumnSchema, ForeignKeySchema, IndexSchema, TableSchema};

/// Generates backend-specific SQL from derived table schema metadata.
pub trait Generator {
    /// Generate a full set of SQL statements for creating a table and its indexes.
    fn create_table(&self, schema: &TableSchema) -> Vec<String> {
        let mut statements = vec![self.create_table_statement(schema)];
        statements.extend(
            schema
                .indexes
                .iter()
                .map(|index| self.create_index_statement(schema, index)),
        );
        statements
    }

    /// Generate the backend-specific `CREATE TABLE` statement.
    fn create_table_statement(&self, schema: &TableSchema) -> String;

    /// Generate the backend-specific `CREATE INDEX` statement for one index.
    fn create_index_statement(&self, table: &TableSchema, index: &IndexSchema) -> String;

    /// Generate the backend-specific SQL fragment for one column definition.
    fn column_definition(&self, column: &ColumnSchema) -> String;

    /// Generate the backend-specific SQL fragment for one foreign-key reference.
    fn foreign_key_reference(&self, reference: &ForeignKeySchema) -> String;
}
