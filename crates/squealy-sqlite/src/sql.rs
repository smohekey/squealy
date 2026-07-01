//! SQLite DDL rendering for an owned [`DatabaseModel`].
//!
//! Structurally parallel to the PostgreSQL/MySQL renderers but in SQLite's dialect, which differs in
//! ways that are not cosmetic:
//! - **No schemas/namespaces** — tables are rendered unqualified (SQLite has no `CREATE SCHEMA`).
//! - **Type affinities** — every neutral [`SqlType`] maps to one of `INTEGER`/`REAL`/`TEXT`/`BLOB`/
//!   `NUMERIC` (SQLite is dynamically typed; the column type only sets affinity).
//! - **Identity** is `INTEGER PRIMARY KEY AUTOINCREMENT` written **on the column** (the rowid alias),
//!   so an auto-increment column carries the primary key inline instead of a table-level constraint.
//! - **Foreign keys are inline** in `CREATE TABLE` — SQLite cannot `ALTER TABLE … ADD CONSTRAINT`.
//! - **Constraints are rendered unnamed** — SQLite does not round-trip user constraint names.

use std::io::{self, Write};

use squealy::{
    CheckModel, ColumnModel, DatabaseModel, DefaultValue, ForeignKeyModel, IndexDirection,
    IndexModel, SqlType, TableModel,
};

/// Renders ordered create-from-scratch DDL for a whole model. Statements are `;`-terminated and
/// newline-separated: tables (with inline PK/unique/check/foreign-keys), then indexes, then views in
/// dependency order. SQLite has no schemas, so schema names are dropped and all tables are flattened.
pub(crate) fn write_database(model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
    // View rendering is deferred. The shared view-body renderer emits schema-qualified source names
    // (`"app"."users"`) and PostgreSQL/MySQL scalar-function spellings (e.g. `CHAR_LENGTH`) that SQLite
    // rejects; rendering views correctly needs SQLite-specific `Dialect` seams (schema suppression +
    // scalar-function names) — a later slice. Error rather than emit broken DDL.
    if model.schemas.iter().any(|schema| !schema.views.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite view rendering is not supported yet",
        ));
    }

    // SQLite keeps tables and indexes in one database-wide object namespace (there are no schemas), so
    // once schemas are flattened every table and index name must be unique — including a table name
    // that matches an index name. A model that relies on schema/table scoping for those names is valid
    // for the schema-aware backends but cannot be represented in SQLite; reject it before rendering
    // duplicate `CREATE TABLE`/`CREATE INDEX` statements. Names are compared case-insensitively
    // (ASCII-folded), matching how SQLite compares object identifiers even when quoted, and tables are
    // inserted first so an index that collides with a table is reported too.
    let mut seen = std::collections::HashSet::new();
    for schema in &model.schemas {
        for table in &schema.tables {
            if !seen.insert(table.name.to_ascii_lowercase()) {
                return Err(object_name_collision(&table.name));
            }
        }
    }
    for schema in &model.schemas {
        for table in &schema.tables {
            for index in &table.indexes {
                if !seen.insert(index.name.to_ascii_lowercase()) {
                    return Err(object_name_collision(&index.name));
                }
            }
        }
    }

    let mut first = true;

    for schema in &model.schemas {
        for table in &schema.tables {
            statement(writer, &mut first)?;
            write_create_table(table, writer)?;
        }
    }

    for schema in &model.schemas {
        for table in &schema.tables {
            for index in &table.indexes {
                statement(writer, &mut first)?;
                write_create_index(&table.name, index, writer)?;
            }
        }
    }

    if !first {
        writer.write_all(b";")?;
    }
    Ok(())
}

fn object_name_collision(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "SQLite has no schemas and keeps tables and indexes in one object namespace, so `{name}` \
             is not unique after flattening schemas"
        ),
    )
}

fn statement(writer: &mut impl Write, first: &mut bool) -> io::Result<()> {
    if *first {
        *first = false;
    } else {
        writer.write_all(b";\n")?;
    }
    Ok(())
}

fn write_create_table(table: &TableModel, writer: &mut impl Write) -> io::Result<()> {
    // SQLite carries auto-increment as `INTEGER PRIMARY KEY AUTOINCREMENT` on a single column (the
    // rowid alias), so it must be the sole, single-column primary key — and the table-level primary
    // key constraint is then omitted.
    let autoincrement_column = autoincrement_column(table)?;

    writer.write_all(b"CREATE TABLE ")?;
    write_quoted_ident(&table.name, writer)?;
    writer.write_all(b" (\n")?;

    let mut first_entry = true;
    for column in &table.columns {
        entry(writer, &mut first_entry)?;
        if Some(column.name.as_str()) == autoincrement_column {
            write_autoincrement_column(column, writer)?;
        } else {
            write_column(column, writer)?;
        }
    }
    // The auto-increment column already declared the primary key inline; only emit a table-level
    // primary key when there is no auto-increment column to carry it.
    if let (None, Some(primary_key)) = (autoincrement_column, &table.primary_key) {
        entry(writer, &mut first_entry)?;
        writer.write_all(b"PRIMARY KEY (")?;
        write_quoted_ident_list(&primary_key.columns, writer)?;
        writer.write_all(b")")?;
    }
    for unique in &table.uniques {
        entry(writer, &mut first_entry)?;
        writer.write_all(b"UNIQUE (")?;
        write_quoted_ident_list(&unique.columns, writer)?;
        writer.write_all(b")")?;
    }
    for check in &table.checks {
        entry(writer, &mut first_entry)?;
        write_check(check, writer)?;
    }
    for foreign_key in &table.foreign_keys {
        entry(writer, &mut first_entry)?;
        write_foreign_key(foreign_key, writer)?;
    }

    writer.write_all(b"\n)")
}

/// The single auto-increment column's name, validated for SQLite's `INTEGER PRIMARY KEY AUTOINCREMENT`
/// rule: at most one, and it must be exactly the (single-column) primary key.
fn autoincrement_column(table: &TableModel) -> io::Result<Option<&str>> {
    let mut identity_columns = table
        .columns
        .iter()
        .filter(|column| column.identity.is_some());
    let Some(column) = identity_columns.next() else {
        return Ok(None);
    };
    if identity_columns.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite supports at most one AUTOINCREMENT column per table",
        ));
    }
    let is_sole_primary_key = table
        .primary_key
        .as_ref()
        .is_some_and(|pk| pk.columns.len() == 1 && pk.columns[0] == column.name);
    if !is_sole_primary_key {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite AUTOINCREMENT requires the auto-increment column to be the table's \
             single-column INTEGER primary key",
        ));
    }
    // The column is rewritten to `INTEGER PRIMARY KEY AUTOINCREMENT`, so its declared type must be an
    // integer — otherwise a (e.g. packaged/hand-written) model with a `TEXT` identity would be created
    // with a silently different column type.
    if !is_integer_type(&column.ty) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite AUTOINCREMENT requires an integer-typed primary key column",
        ));
    }
    Ok(Some(column.name.as_str()))
}

/// Whether the neutral type is a true integer (the only types valid as a SQLite
/// `INTEGER PRIMARY KEY AUTOINCREMENT` rowid alias). `Bool` is deliberately excluded.
fn is_integer_type(ty: &SqlType) -> bool {
    matches!(
        ty,
        SqlType::I8
            | SqlType::I16
            | SqlType::I32
            | SqlType::I64
            | SqlType::I128
            | SqlType::Isize
            | SqlType::U8
            | SqlType::U16
            | SqlType::U32
            | SqlType::U64
            | SqlType::U128
            | SqlType::Usize
    )
}

fn entry(writer: &mut impl Write, first: &mut bool) -> io::Result<()> {
    if *first {
        *first = false;
        writer.write_all(b"  ")
    } else {
        writer.write_all(b",\n  ")
    }
}

/// Renders the auto-increment column as `"name" INTEGER PRIMARY KEY AUTOINCREMENT` — the SQLite rowid
/// alias. `PRIMARY KEY` implies `NOT NULL`, and identity columns carry no default.
fn write_autoincrement_column(column: &ColumnModel, writer: &mut impl Write) -> io::Result<()> {
    write_quoted_ident(&column.name, writer)?;
    writer.write_all(b" INTEGER PRIMARY KEY AUTOINCREMENT")
}

fn write_column(column: &ColumnModel, writer: &mut impl Write) -> io::Result<()> {
    if column.generated.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "SQLite generated columns are not supported yet (column `{}`)",
                column.name
            ),
        ));
    }
    write_quoted_ident(&column.name, writer)?;
    writer.write_all(b" ")?;
    writer.write_all(sqlite_affinity(&column.ty).as_bytes())?;
    if let Some(collation) = &column.collation {
        writer.write_all(b" COLLATE ")?;
        writer.write_all(collation.as_bytes())?;
    }
    if !column.nullable {
        writer.write_all(b" NOT NULL")?;
    }
    if let Some(default) = &column.default {
        writer.write_all(b" DEFAULT ")?;
        write_default_value(default, writer)?;
    }
    // SQLite's `BLOB` affinity has no fixed width, so a `[u8; N]`/`FixedBytes(N)` column enforces it
    // with a column `CHECK (length(CAST("col" AS BLOB)) = N)` — the equivalent of the PostgreSQL
    // backend's generated `octet_length` check (a `NULL` value passes, matching a nullable column). The
    // `CAST(… AS BLOB)` makes the check byte-based even if a value is stored with text affinity, where a
    // bare `length()` would count characters rather than bytes.
    if let SqlType::FixedBytes(width) = &column.ty {
        writer.write_all(b" CHECK (length(CAST(")?;
        write_quoted_ident(&column.name, writer)?;
        write!(writer, " AS BLOB)) = {width})")?;
    }
    Ok(())
}

fn write_check(check: &CheckModel, writer: &mut impl Write) -> io::Result<()> {
    if check.validation.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support constraint validation metadata",
        ));
    }
    if check.enforcement.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support check constraint enforcement metadata",
        ));
    }
    // Rendered unnamed: SQLite does not round-trip user constraint names.
    write!(writer, "CHECK ({})", check.expression)
}

fn write_foreign_key(foreign_key: &ForeignKeyModel, writer: &mut impl Write) -> io::Result<()> {
    if foreign_key.match_type.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support foreign key MATCH clauses",
        ));
    }
    if foreign_key.deferrability.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite deferrable foreign keys are not supported yet",
        ));
    }
    if foreign_key.validation.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support foreign key validation metadata",
        ));
    }
    if foreign_key.enforcement.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support foreign key enforcement metadata",
        ));
    }
    // Rendered inline and unnamed (SQLite cannot `ALTER TABLE … ADD CONSTRAINT`, and does not
    // round-trip foreign key constraint names). The referenced table is unqualified.
    writer.write_all(b"FOREIGN KEY (")?;
    write_quoted_ident_list(&foreign_key.columns, writer)?;
    writer.write_all(b") REFERENCES ")?;
    write_quoted_ident(&foreign_key.references_table, writer)?;
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

fn write_create_index(table: &str, index: &IndexModel, writer: &mut impl Write) -> io::Result<()> {
    if !index.expressions.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite expression indexes are not supported yet",
        ));
    }
    if !index.include_columns.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support covering index include columns",
        ));
    }
    if !index.operator_classes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support index operator classes",
        ));
    }
    if !index.collations.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite index collation overrides are not supported yet",
        ));
    }
    if !index.nulls.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite index null ordering is not supported yet",
        ));
    }
    if index.method.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support index access methods",
        ));
    }

    writer.write_all(b"CREATE ")?;
    if index.unique {
        writer.write_all(b"UNIQUE ")?;
    }
    writer.write_all(b"INDEX ")?;
    write_quoted_ident(&index.name, writer)?;
    writer.write_all(b" ON ")?;
    write_quoted_ident(table, writer)?;
    writer.write_all(b" (")?;
    for (position, column) in index.columns.iter().enumerate() {
        if position > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(column, writer)?;
        match index.directions.get(position) {
            Some(IndexDirection::Asc) => writer.write_all(b" ASC")?,
            Some(IndexDirection::Desc) => writer.write_all(b" DESC")?,
            None => {}
        }
    }
    writer.write_all(b")")?;
    // SQLite supports partial indexes (a `WHERE` predicate), unlike MySQL.
    if let Some(predicate) = &index.predicate {
        write!(writer, " WHERE {predicate}")?;
    }
    Ok(())
}

/// The SQLite type affinity for a neutral [`SqlType`]. SQLite is dynamically typed, so the column type
/// only assigns one of five affinities; this is reused by [`SqliteDialect::write_cast_type`].
fn sqlite_affinity(ty: &SqlType) -> &str {
    match ty {
        SqlType::Bool
        | SqlType::I8
        | SqlType::I16
        | SqlType::I32
        | SqlType::I64
        | SqlType::I128
        | SqlType::Isize
        | SqlType::U8
        | SqlType::U16
        | SqlType::U32
        | SqlType::U64
        | SqlType::U128
        | SqlType::Usize => "INTEGER",
        SqlType::F32 | SqlType::F64 => "REAL",
        SqlType::Decimal { .. } => "NUMERIC",
        SqlType::String
        | SqlType::Varchar(_)
        | SqlType::Char(_)
        | SqlType::Text
        | SqlType::Date
        | SqlType::Time { .. }
        | SqlType::Timestamp { .. }
        | SqlType::Uuid
        | SqlType::Json
        | SqlType::Jsonb => "TEXT",
        SqlType::Bytes | SqlType::FixedBytes(_) => "BLOB",
        SqlType::Raw(raw) => raw.as_str(),
    }
}

fn write_default_value(default: &DefaultValue, writer: &mut impl Write) -> io::Result<()> {
    match default {
        DefaultValue::Null => writer.write_all(b"NULL"),
        DefaultValue::Int(value) => write!(writer, "{value}"),
        DefaultValue::UInt(value) => write!(writer, "{value}"),
        DefaultValue::Float(value) => write!(writer, "{value}"),
        // SQLite has no boolean literal; booleans are stored as integers.
        DefaultValue::Bool(true) => writer.write_all(b"1"),
        DefaultValue::Bool(false) => writer.write_all(b"0"),
        DefaultValue::Text(value) => write_quoted_text(value, writer),
        DefaultValue::CurrentTimestamp => writer.write_all(b"CURRENT_TIMESTAMP"),
        DefaultValue::CurrentDate => writer.write_all(b"CURRENT_DATE"),
        DefaultValue::CurrentTime => writer.write_all(b"CURRENT_TIME"),
        DefaultValue::Raw(value) => writer.write_all(value.as_bytes()),
    }
}

// --- SQLite identifier/value quoting (double quotes) ---

/// Quotes an identifier with double quotes, doubling any embedded double quote.
fn write_quoted_ident(value: &str, writer: &mut impl Write) -> io::Result<()> {
    write_delimited(value, '"', writer)
}

fn write_quoted_text(value: &str, writer: &mut impl Write) -> io::Result<()> {
    write_delimited(value, '\'', writer)
}

fn write_quoted_ident_list(columns: &[String], writer: &mut impl Write) -> io::Result<()> {
    for (position, column) in columns.iter().enumerate() {
        if position > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(column, writer)?;
    }
    Ok(())
}

fn write_delimited(value: &str, delimiter: char, writer: &mut impl Write) -> io::Result<()> {
    let mut encoded = [0u8; 4];
    let delim = delimiter.encode_utf8(&mut encoded).as_bytes();
    writer.write_all(delim)?;
    let mut start = 0;
    for (index, _) in value.match_indices(delimiter) {
        writer.write_all(&value.as_bytes()[start..index])?;
        writer.write_all(delim)?;
        writer.write_all(delim)?;
        start = index + delimiter.len_utf8();
    }
    writer.write_all(&value.as_bytes()[start..])?;
    writer.write_all(delim)
}

/// SQLite's [`Dialect`](squealy::Dialect): `?` placeholders, double-quoted identifiers, and SQLite
/// `CAST` affinity names. Everything else uses the trait defaults, which already match SQLite —
/// integer division needs a float cast, `DEFAULT VALUES` empty inserts, `NULLS FIRST`/`LAST`,
/// `ON CONFLICT … DO UPDATE/NOTHING` upserts, and `UPDATE … FROM`. The query layer (a later slice)
/// renders queries through this; it is defined now so the dialect mapping lives with the DDL renderer.
// Wired up by the SQLite query layer (a later slice); view-body rendering is deferred until the
// SQLite-specific scalar-function / schema-suppression seams exist.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SqliteDialect;

impl squealy::Dialect for SqliteDialect {
    fn write_placeholder(&self, _index: usize, writer: &mut dyn Write) -> io::Result<()> {
        // SQLite uses positional `?` placeholders.
        writer.write_all(b"?")
    }

    fn write_quoted_ident(&self, ident: &str, mut writer: &mut dyn Write) -> io::Result<()> {
        write_quoted_ident(ident, &mut writer)
    }

    fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
        // `CAST(expr AS <type>)` uses SQLite's affinity names, the same mapping as the column type.
        writer.write_all(sqlite_affinity(ty).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::sqlite_affinity;
    use squealy::SqlType;

    #[test]
    fn affinities_map_neutral_types() {
        assert_eq!(sqlite_affinity(&SqlType::I32), "INTEGER");
        assert_eq!(sqlite_affinity(&SqlType::I64), "INTEGER");
        assert_eq!(sqlite_affinity(&SqlType::Bool), "INTEGER");
        assert_eq!(sqlite_affinity(&SqlType::U64), "INTEGER");
        assert_eq!(sqlite_affinity(&SqlType::F64), "REAL");
        assert_eq!(
            sqlite_affinity(&SqlType::Decimal {
                precision: 10,
                scale: 2
            }),
            "NUMERIC"
        );
        assert_eq!(sqlite_affinity(&SqlType::String), "TEXT");
        assert_eq!(sqlite_affinity(&SqlType::Varchar(64)), "TEXT");
        assert_eq!(sqlite_affinity(&SqlType::Text), "TEXT");
        assert_eq!(sqlite_affinity(&SqlType::Timestamp { tz: true }), "TEXT");
        assert_eq!(sqlite_affinity(&SqlType::Uuid), "TEXT");
        assert_eq!(sqlite_affinity(&SqlType::Json), "TEXT");
        assert_eq!(sqlite_affinity(&SqlType::Bytes), "BLOB");
        assert_eq!(sqlite_affinity(&SqlType::FixedBytes(16)), "BLOB");
        assert_eq!(
            sqlite_affinity(&SqlType::Raw("GEOMETRY".to_owned())),
            "GEOMETRY"
        );
    }
}
