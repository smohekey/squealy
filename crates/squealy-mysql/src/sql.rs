//! MySQL DDL rendering for an owned [`DatabaseModel`].
//!
//! Structurally parallel to the PostgreSQL renderer, differing only in dialect: backtick identifier
//! quoting, MySQL type names (incl. unsigned integers), `AUTO_INCREMENT` identity, and `VARCHAR`-backed
//! strings. The parallelism is itself a finding — a future refactor could share a dialect-parameterized
//! writer — but each backend keeps its own renderer for now.

use std::io::{self, Write};

use squealy::{
    CheckModel, ColumnModel, DatabaseModel, DefaultValue, ForeignKeyModel, GeneratedStorage,
    IndexModel, SqlType, TableModel,
};

/// Renders ordered create-from-scratch DDL for a whole model. Statements are `;`-terminated and
/// newline-separated: namespaces, then tables (with inline PK/unique/check), then indexes, then
/// foreign keys as separate `ALTER TABLE … ADD CONSTRAINT`.
pub(crate) fn write_database(model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
    let mut first = true;

    for schema in &model.schemas {
        if let Some(name) = schema.name.as_deref() {
            statement(writer, &mut first)?;
            writer.write_all(b"CREATE SCHEMA IF NOT EXISTS ")?;
            write_quoted_ident(name, writer)?;
        }
    }

    for schema in &model.schemas {
        for table in &schema.tables {
            statement(writer, &mut first)?;
            write_create_table(schema.name.as_deref(), table, writer)?;
        }
    }

    for schema in &model.schemas {
        for table in &schema.tables {
            for index in &table.indexes {
                statement(writer, &mut first)?;
                write_create_index(schema.name.as_deref(), &table.name, index, writer)?;
            }
        }
    }

    for schema in &model.schemas {
        for table in &schema.tables {
            for foreign_key in &table.foreign_keys {
                statement(writer, &mut first)?;
                write_add_foreign_key(schema.name.as_deref(), &table.name, foreign_key, writer)?;
            }
        }
    }

    if !first {
        writer.write_all(b";")?;
    }
    Ok(())
}

fn statement(writer: &mut impl Write, first: &mut bool) -> io::Result<()> {
    if *first {
        *first = false;
    } else {
        writer.write_all(b";\n")?;
    }
    Ok(())
}

fn write_create_table(
    schema: Option<&str>,
    table: &TableModel,
    writer: &mut impl Write,
) -> io::Result<()> {
    writer.write_all(b"CREATE TABLE ")?;
    write_qualified_name(schema, &table.name, writer)?;
    writer.write_all(b" (\n")?;

    let mut first_entry = true;
    for column in &table.columns {
        entry(writer, &mut first_entry)?;
        write_column(column, writer)?;
    }
    if let Some(primary_key) = &table.primary_key {
        entry(writer, &mut first_entry)?;
        write_named_constraint(
            "PRIMARY KEY",
            &primary_key.name,
            &primary_key.columns,
            writer,
        )?;
    }
    for unique in &table.uniques {
        entry(writer, &mut first_entry)?;
        write_named_constraint("UNIQUE", &unique.name, &unique.columns, writer)?;
    }
    for check in &table.checks {
        entry(writer, &mut first_entry)?;
        write_check(check, writer)?;
    }

    writer.write_all(b"\n)")
}

fn entry(writer: &mut impl Write, first: &mut bool) -> io::Result<()> {
    if *first {
        *first = false;
        writer.write_all(b"  ")
    } else {
        writer.write_all(b",\n  ")
    }
}

fn write_column(column: &ColumnModel, writer: &mut impl Write) -> io::Result<()> {
    write_quoted_ident(&column.name, writer)?;
    writer.write_all(b" ")?;
    write_mysql_sql_type(&column.ty, writer)?;
    if !column.nullable {
        writer.write_all(b" NOT NULL")?;
    }
    if let Some(default) = &column.default {
        writer.write_all(b" DEFAULT ")?;
        write_default_value(default, writer)?;
    }
    if column.identity.is_some() {
        writer.write_all(b" AUTO_INCREMENT")?;
    }
    if let Some(generated) = &column.generated {
        writer.write_all(b" GENERATED ALWAYS AS (")?;
        writer.write_all(generated.expression.as_bytes())?;
        writer.write_all(b")")?;
        match generated.storage {
            GeneratedStorage::Virtual | GeneratedStorage::Unknown => {
                writer.write_all(b" VIRTUAL")?
            }
            GeneratedStorage::Stored => writer.write_all(b" STORED")?,
        }
    }
    Ok(())
}

/// Renders the neutral [`SqlType`] as a MySQL DDL type.
///
/// MySQL differs from PostgreSQL in several ways the neutral model surfaces: native unsigned integers,
/// a `TINYINT(1)` boolean, no unbounded `text` usable in keys (so bare `String` becomes `VARCHAR(255)`),
/// no native `uuid` (rendered `CHAR(36)`) and only `JSON` (so `Jsonb` also renders `JSON`).
fn write_mysql_sql_type(ty: &SqlType, writer: &mut impl Write) -> io::Result<()> {
    let name = match ty {
        SqlType::Bool => "TINYINT(1)",
        SqlType::I8 => "TINYINT",
        SqlType::I16 => "SMALLINT",
        SqlType::I32 => "INT",
        SqlType::I64 | SqlType::I128 | SqlType::Isize => "BIGINT",
        SqlType::U8 => "TINYINT UNSIGNED",
        SqlType::U16 => "SMALLINT UNSIGNED",
        SqlType::U32 => "INT UNSIGNED",
        SqlType::U64 | SqlType::U128 | SqlType::Usize => "BIGINT UNSIGNED",
        SqlType::F32 => "FLOAT",
        SqlType::F64 => "DOUBLE",
        SqlType::String => "VARCHAR(255)",
        SqlType::Varchar(length) => return write!(writer, "VARCHAR({length})"),
        SqlType::Char(length) => return write!(writer, "CHAR({length})"),
        SqlType::Text => "TEXT",
        SqlType::Decimal { precision, scale } => {
            return write!(writer, "DECIMAL({precision},{scale})");
        }
        SqlType::Date => "DATE",
        SqlType::Time { .. } => "TIME",
        SqlType::Timestamp { tz: true } => "TIMESTAMP",
        SqlType::Timestamp { tz: false } => "DATETIME",
        SqlType::Uuid => "CHAR(36)",
        SqlType::Json | SqlType::Jsonb => "JSON",
        SqlType::Bytes => "BLOB",
        SqlType::Raw(raw) => raw.as_str(),
    };
    writer.write_all(name.as_bytes())
}

fn write_default_value(default: &DefaultValue, writer: &mut impl Write) -> io::Result<()> {
    match default {
        DefaultValue::Null => writer.write_all(b"NULL"),
        DefaultValue::Int(value) => write!(writer, "{value}"),
        DefaultValue::UInt(value) => write!(writer, "{value}"),
        DefaultValue::Float(value) => write!(writer, "{value}"),
        DefaultValue::Text(value) => write_quoted_text(value, writer),
        DefaultValue::Bool(true) => writer.write_all(b"TRUE"),
        DefaultValue::Bool(false) => writer.write_all(b"FALSE"),
        DefaultValue::CurrentTimestamp => writer.write_all(b"CURRENT_TIMESTAMP"),
        DefaultValue::CurrentDate => writer.write_all(b"(CURRENT_DATE)"),
        DefaultValue::CurrentTime => writer.write_all(b"(CURRENT_TIME)"),
        DefaultValue::Raw(value) => writer.write_all(value.as_bytes()),
    }
}

fn write_named_constraint(
    kind: &str,
    name: &str,
    columns: &[String],
    writer: &mut impl Write,
) -> io::Result<()> {
    writer.write_all(b"CONSTRAINT ")?;
    write_quoted_ident(name, writer)?;
    write!(writer, " {kind} (")?;
    write_quoted_ident_list(columns, writer)?;
    writer.write_all(b")")
}

fn write_check(check: &CheckModel, writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"CONSTRAINT ")?;
    write_quoted_ident(&check.name, writer)?;
    write!(writer, " CHECK ({})", check.expression)
}

fn write_create_index(
    schema: Option<&str>,
    table: &str,
    index: &IndexModel,
    writer: &mut impl Write,
) -> io::Result<()> {
    if index.predicate.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support partial index predicates",
        ));
    }
    if !index.expressions.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL expression indexes are not supported by squealy yet",
        ));
    }
    if !index.include_columns.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support covering index include columns",
        ));
    }

    writer.write_all(b"CREATE ")?;
    if index.unique {
        writer.write_all(b"UNIQUE ")?;
    }
    writer.write_all(b"INDEX ")?;
    write_quoted_ident(&index.name, writer)?;
    if let Some(method) = &index.method {
        writer.write_all(b" USING ")?;
        writer.write_all(method.mysql_sql().as_bytes())?;
    }
    writer.write_all(b" ON ")?;
    write_qualified_name(schema, table, writer)?;
    writer.write_all(b" (")?;
    write_index_columns(index, writer)?;
    writer.write_all(b")")?;
    Ok(())
}

fn write_index_columns(index: &IndexModel, writer: &mut impl Write) -> io::Result<()> {
    for (position, column) in index.columns.iter().enumerate() {
        if position > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(column, writer)?;
        match index.directions.get(position) {
            Some(squealy::IndexDirection::Asc) => writer.write_all(b" ASC")?,
            Some(squealy::IndexDirection::Desc) => writer.write_all(b" DESC")?,
            None => {}
        }
    }
    Ok(())
}

fn write_add_foreign_key(
    schema: Option<&str>,
    table: &str,
    foreign_key: &ForeignKeyModel,
    writer: &mut impl Write,
) -> io::Result<()> {
    writer.write_all(b"ALTER TABLE ")?;
    write_qualified_name(schema, table, writer)?;
    writer.write_all(b" ADD CONSTRAINT ")?;
    write_quoted_ident(&foreign_key.name, writer)?;
    writer.write_all(b" FOREIGN KEY (")?;
    write_quoted_ident_list(&foreign_key.columns, writer)?;
    writer.write_all(b") REFERENCES ")?;
    write_qualified_name(
        foreign_key.references_schema.as_deref(),
        &foreign_key.references_table,
        writer,
    )?;
    writer.write_all(b" (")?;
    write_quoted_ident_list(&foreign_key.references_columns, writer)?;
    writer.write_all(b")")?;
    if let Some(on_delete) = &foreign_key.on_delete {
        write!(writer, " ON DELETE {}", on_delete.as_sql())?;
    }
    if let Some(on_update) = &foreign_key.on_update {
        write!(writer, " ON UPDATE {}", on_update.as_sql())?;
    }
    Ok(())
}

// --- MySQL identifier/value quoting (backticks) ---

fn write_qualified_name(
    schema: Option<&str>,
    name: &str,
    writer: &mut impl Write,
) -> io::Result<()> {
    if let Some(schema) = schema {
        write_quoted_ident(schema, writer)?;
        writer.write_all(b".")?;
    }
    write_quoted_ident(name, writer)
}

/// Quotes an identifier with backticks, doubling any embedded backtick. Writes whole UTF-8 slices so
/// validating writers accept multibyte identifiers.
fn write_quoted_ident(value: &str, writer: &mut impl Write) -> io::Result<()> {
    write_delimited(value, '`', writer)
}

fn write_quoted_text(value: &str, writer: &mut impl Write) -> io::Result<()> {
    write_delimited(value, '\'', writer)
}

fn write_delimited(value: &str, delimiter: char, writer: &mut impl Write) -> io::Result<()> {
    let mut encoded = [0u8; 4];
    let delim = delimiter.encode_utf8(&mut encoded).as_bytes();
    writer.write_all(delim)?;
    let mut start = 0;
    for (index, _) in value.match_indices(delimiter) {
        writer.write_all(value[start..index].as_bytes())?;
        writer.write_all(delim)?;
        writer.write_all(delim)?;
        start = index + delimiter.len_utf8();
    }
    writer.write_all(value[start..].as_bytes())?;
    writer.write_all(delim)
}

fn write_quoted_ident_list(columns: &[String], writer: &mut impl Write) -> io::Result<()> {
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(column, writer)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_type(ty: SqlType) -> String {
        let mut out = Vec::new();
        write_mysql_sql_type(&ty, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn mysql_types_map_to_mysql_ddl_types() {
        let cases = [
            (SqlType::Bool, "TINYINT(1)"),
            (SqlType::I8, "TINYINT"),
            (SqlType::I16, "SMALLINT"),
            (SqlType::I32, "INT"),
            (SqlType::I64, "BIGINT"),
            (SqlType::U8, "TINYINT UNSIGNED"),
            (SqlType::U32, "INT UNSIGNED"),
            (SqlType::U64, "BIGINT UNSIGNED"),
            (SqlType::F32, "FLOAT"),
            (SqlType::F64, "DOUBLE"),
            (SqlType::String, "VARCHAR(255)"),
            (SqlType::Raw("GEOMETRY".to_owned()), "GEOMETRY"),
        ];

        for (ty, expected) in cases {
            assert_eq!(render_type(ty), expected);
        }
    }

    #[test]
    fn mysql_renders_structured_types() {
        assert_eq!(render_type(SqlType::Varchar(64)), "VARCHAR(64)");
        assert_eq!(render_type(SqlType::Char(2)), "CHAR(2)");
        assert_eq!(render_type(SqlType::Text), "TEXT");
        assert_eq!(
            render_type(SqlType::Decimal {
                precision: 10,
                scale: 2
            }),
            "DECIMAL(10,2)"
        );
        assert_eq!(render_type(SqlType::Date), "DATE");
        assert_eq!(render_type(SqlType::Time { tz: false }), "TIME");
        assert_eq!(render_type(SqlType::Timestamp { tz: false }), "DATETIME");
        assert_eq!(render_type(SqlType::Timestamp { tz: true }), "TIMESTAMP");
        assert_eq!(render_type(SqlType::Uuid), "CHAR(36)");
        assert_eq!(render_type(SqlType::Json), "JSON");
        assert_eq!(render_type(SqlType::Jsonb), "JSON");
        assert_eq!(render_type(SqlType::Bytes), "BLOB");
    }
}
