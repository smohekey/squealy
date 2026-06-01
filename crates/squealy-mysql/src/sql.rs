//! MySQL DDL rendering for an owned [`DatabaseModel`].
//!
//! Structurally parallel to the PostgreSQL renderer, differing only in dialect: backtick identifier
//! quoting, MySQL type names (incl. unsigned integers), `AUTO_INCREMENT` identity, and `VARCHAR`-backed
//! strings. The parallelism is itself a finding — a future refactor could share a dialect-parameterized
//! writer — but each backend keeps its own renderer for now.

use std::io::{self, Write};

use squealy::{
    CheckModel, ColumnModel, DatabaseModel, DefaultValue, ForeignKeyModel, IndexModel, SqlType,
    TableModel,
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
    write!(writer, " {}", mysql_sql_type(&column.ty))?;
    if !column.nullable {
        writer.write_all(b" NOT NULL")?;
    }
    if let Some(default) = &column.default {
        writer.write_all(b" DEFAULT ")?;
        write_default_value(default, writer)?;
    }
    if column.auto_increment {
        writer.write_all(b" AUTO_INCREMENT")?;
    }
    Ok(())
}

/// Maps the neutral [`SqlType`] to a MySQL DDL type.
///
/// Unlike PostgreSQL, MySQL has native unsigned integers, no first-class unbounded `text` usable in
/// keys, and a `TINYINT(1)` boolean — so `String` becomes `VARCHAR(255)` (index-safe) here. That
/// `SqlType::String` is "unbounded text" with no length is a neutral-model nuance each backend renders
/// its own way; introspection/diff will want a length-carrying string type eventually.
fn mysql_sql_type(ty: &SqlType) -> &str {
    match ty {
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
        SqlType::Raw(raw) => raw.as_str(),
    }
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
    writer.write_all(b"CREATE ")?;
    if index.unique {
        writer.write_all(b"UNIQUE ")?;
    }
    writer.write_all(b"INDEX ")?;
    write_quoted_ident(&index.name, writer)?;
    writer.write_all(b" ON ")?;
    write_qualified_name(schema, table, writer)?;
    writer.write_all(b" (")?;
    write_quoted_ident_list(&index.columns, writer)?;
    writer.write_all(b")")
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
        write!(writer, " ON DELETE {on_delete}")?;
    }
    if let Some(on_update) = &foreign_key.on_update {
        write!(writer, " ON UPDATE {on_update}")?;
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
