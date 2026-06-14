//! MySQL DDL rendering for an owned [`DatabaseModel`].
//!
//! Structurally parallel to the PostgreSQL renderer, differing only in dialect: backtick identifier
//! quoting, MySQL type names (incl. unsigned integers), `AUTO_INCREMENT` identity, and `VARCHAR`-backed
//! strings. The parallelism is itself a finding — a future refactor could share a dialect-parameterized
//! writer — but each backend keeps its own renderer for now.

use std::io::{self, Write};

use squealy::{
    CheckModel, ColumnDefault, ColumnModel, DatabaseModel, DatabasePlan, DatabasePlanStep,
    DefaultValue, ForeignKeyModel, GeneratedStorage, IndexModel, SqlType, Table, TableModel,
    TablePlanStep,
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

/// Renders an ordered incremental DDL plan.
pub(crate) fn write_plan(plan: &DatabasePlan, writer: &mut impl Write) -> io::Result<()> {
    let mut first = true;
    if plan.steps.iter().any(plan_step_has_refactor_id) {
        statement(writer, &mut first)?;
        write_create_refactor_log_schema(writer)?;
        statement(writer, &mut first)?;
        write_create_refactor_log_table(writer)?;
    }
    for step in &plan.steps {
        write_plan_step(step, writer, &mut first)?;
    }
    write_deferred_foreign_keys(plan, writer, &mut first)?;
    if !first {
        writer.write_all(b";")?;
    }
    Ok(())
}

/// Emits the `ADD FOREIGN KEY` constraints for every table created in `plan`, deferred until all
/// `CreateTable` steps have rendered, so a foreign key pointing at a later-created table does not
/// reference a table that does not exist yet (a single plan can create several tables in name order).
fn write_deferred_foreign_keys(
    plan: &DatabasePlan,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    for step in &plan.steps {
        if let DatabasePlanStep::CreateTable { schema, table } = step {
            for foreign_key in &table.foreign_keys {
                statement(writer, first)?;
                write_add_foreign_key(schema.as_deref(), &table.name, foreign_key, writer)?;
            }
        }
    }
    Ok(())
}

fn write_plan_step(
    step: &DatabasePlanStep,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    match step {
        DatabasePlanStep::CreateSchema { schema } => {
            if let Some(schema) = schema.as_deref() {
                statement(writer, first)?;
                writer.write_all(b"CREATE SCHEMA IF NOT EXISTS ")?;
                write_quoted_ident(schema, writer)?;
            }
        }
        DatabasePlanStep::DropSchema { schema } => {
            if let Some(schema) = schema.as_deref() {
                statement(writer, first)?;
                writer.write_all(b"DROP SCHEMA ")?;
                write_quoted_ident(schema, writer)?;
            }
        }
        DatabasePlanStep::CreateTable { schema, table } => {
            statement(writer, first)?;
            write_create_table(schema.as_deref(), table, writer)?;
            write_create_table_extras(schema.as_deref(), table, writer, first)?;
        }
        DatabasePlanStep::DropTable { schema, table } => {
            statement(writer, first)?;
            writer.write_all(b"DROP TABLE ")?;
            write_qualified_name(schema.as_deref(), &table.name, writer)?;
        }
        DatabasePlanStep::RenameTable {
            refactor_id,
            schema,
            from,
            to,
        } => {
            statement(writer, first)?;
            writer.write_all(b"RENAME TABLE ")?;
            write_qualified_name(schema.as_deref(), from, writer)?;
            writer.write_all(b" TO ")?;
            write_qualified_name(schema.as_deref(), to, writer)?;
            if let Some(refactor_id) = refactor_id {
                statement(writer, first)?;
                write_record_refactor(refactor_id, writer)?;
            }
        }
        DatabasePlanStep::AlterTable {
            schema,
            table,
            change,
        } => write_table_plan_step(schema.as_deref(), table, change, writer, first)?,
    }
    Ok(())
}

fn write_create_table_extras(
    schema: Option<&str>,
    table: &TableModel,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    for index in &table.indexes {
        statement(writer, first)?;
        write_create_index(schema, &table.name, index, writer)?;
    }
    Ok(())
}

fn plan_step_has_refactor_id(step: &DatabasePlanStep) -> bool {
    match step {
        DatabasePlanStep::RenameTable { refactor_id, .. } => refactor_id.is_some(),
        DatabasePlanStep::AlterTable { change, .. } => match change.as_ref() {
            TablePlanStep::RenameColumn { refactor_id, .. } => refactor_id.is_some(),
            _ => false,
        },
        _ => false,
    }
}

fn write_create_refactor_log_schema(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"CREATE SCHEMA IF NOT EXISTS `__squealy`")
}

fn write_create_refactor_log_table(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(
        b"CREATE TABLE IF NOT EXISTS `__squealy`.`refactors` (\
`id` VARCHAR(255) NOT NULL PRIMARY KEY, \
`applied_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
}

fn write_record_refactor(refactor_id: &str, writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"INSERT IGNORE INTO `__squealy`.`refactors` (`id`) VALUES (")?;
    write_quoted_text(refactor_id, writer)?;
    writer.write_all(b")")
}

fn write_table_plan_step(
    schema: Option<&str>,
    table: &str,
    change: &TablePlanStep,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    statement(writer, first)?;
    match change {
        TablePlanStep::SetTableComment { after, .. } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" COMMENT = ")?;
            write_quoted_text(after.as_deref().unwrap_or(""), writer)?;
        }
        TablePlanStep::AddColumn { column } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ADD COLUMN ")?;
            write_column(column, writer)?;
        }
        TablePlanStep::DropColumn { column } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP COLUMN ")?;
            write_quoted_ident(&column.name, writer)?;
        }
        TablePlanStep::RenameColumn {
            refactor_id,
            from,
            to,
        } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" RENAME COLUMN ")?;
            write_quoted_ident(from, writer)?;
            writer.write_all(b" TO ")?;
            write_quoted_ident(to, writer)?;
            if let Some(refactor_id) = refactor_id {
                statement(writer, first)?;
                write_record_refactor(refactor_id, writer)?;
            }
        }
        TablePlanStep::AddPrimaryKey { constraint } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ADD ")?;
            write_named_constraint("PRIMARY KEY", &constraint.name, &constraint.columns, writer)?;
        }
        TablePlanStep::DropPrimaryKey { .. } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP PRIMARY KEY")?;
        }
        TablePlanStep::AddUnique { constraint } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ADD ")?;
            write_named_constraint("UNIQUE", &constraint.name, &constraint.columns, writer)?;
        }
        TablePlanStep::DropUnique { constraint } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP INDEX ")?;
            write_quoted_ident(&constraint.name, writer)?;
        }
        TablePlanStep::AddForeignKey { foreign_key } => {
            write_add_foreign_key(schema, table, foreign_key, writer)?;
        }
        TablePlanStep::DropForeignKey { foreign_key } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP FOREIGN KEY ")?;
            write_quoted_ident(&foreign_key.name, writer)?;
        }
        TablePlanStep::AddCheck { check } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ADD ")?;
            write_check(check, writer)?;
        }
        TablePlanStep::DropCheck { check } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP CHECK ")?;
            write_quoted_ident(&check.name, writer)?;
        }
        TablePlanStep::AddIndex { index } => {
            write_create_index(schema, table, index, writer)?;
        }
        TablePlanStep::DropIndex { index } => {
            writer.write_all(b"DROP INDEX ")?;
            write_quoted_ident(&index.name, writer)?;
            writer.write_all(b" ON ")?;
            write_qualified_name(schema, table, writer)?;
        }
        TablePlanStep::AlterPrimaryKey { after, .. } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP PRIMARY KEY")?;
            statement(writer, first)?;
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ADD ")?;
            write_named_constraint("PRIMARY KEY", &after.name, &after.columns, writer)?;
        }
        TablePlanStep::AlterUnique { before, after } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP INDEX ")?;
            write_quoted_ident(&before.name, writer)?;
            statement(writer, first)?;
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ADD ")?;
            write_named_constraint("UNIQUE", &after.name, &after.columns, writer)?;
        }
        TablePlanStep::AlterForeignKey { before, after } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP FOREIGN KEY ")?;
            write_quoted_ident(&before.name, writer)?;
            statement(writer, first)?;
            write_add_foreign_key(schema, table, after, writer)?;
        }
        TablePlanStep::AlterCheck { before, after } => {
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" DROP CHECK ")?;
            write_quoted_ident(&before.name, writer)?;
            statement(writer, first)?;
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ADD ")?;
            write_check(after, writer)?;
        }
        TablePlanStep::AlterIndex { before, after } => {
            writer.write_all(b"DROP INDEX ")?;
            write_quoted_ident(&before.name, writer)?;
            writer.write_all(b" ON ")?;
            write_qualified_name(schema, table, writer)?;
            statement(writer, first)?;
            write_create_index(schema, table, after, writer)?;
        }
        // MySQL has no `USING` cast clause; `MODIFY COLUMN` performs the conversion implicitly, so
        // any `type_cast` hint is ignored here.
        TablePlanStep::AlterColumn { before, after, .. } => {
            write_alter_column(schema, table, before, after, writer)?;
        }
    }
    Ok(())
}

fn write_alter_column(
    schema: Option<&str>,
    table: &str,
    before: &ColumnModel,
    after: &ColumnModel,
    writer: &mut impl Write,
) -> io::Result<()> {
    if before.name != after.name {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "MySQL incremental column rename rendering is not supported yet",
        ));
    }

    // `MODIFY COLUMN` re-specifies the whole column, so identity (`AUTO_INCREMENT`) and generated
    // transitions are carried by re-emitting the target definition. MySQL rejects the genuinely
    // impossible transitions (e.g. switching a generated column between VIRTUAL and STORED) at apply
    // time.
    writer.write_all(b"ALTER TABLE ")?;
    write_qualified_name(schema, table, writer)?;
    writer.write_all(b" MODIFY COLUMN ")?;
    write_column(after, writer)
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

    writer.write_all(b"\n)")?;
    if let Some(comment) = &table.comment {
        writer.write_all(b" COMMENT=")?;
        write_quoted_text(comment, writer)?;
    }
    Ok(())
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
    if let Some(comment) = &column.comment {
        writer.write_all(b" COMMENT ")?;
        write_quoted_text(comment, writer)?;
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
    if check.validation.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support constraint validation metadata",
        ));
    }
    if check.enforcement.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL check constraint enforcement metadata is not supported by squealy yet",
        ));
    }
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
    if !index.nulls.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support index null ordering",
        ));
    }
    if !index.operator_classes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support index operator classes",
        ));
    }
    if !index.collations.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support index collation overrides",
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
    if foreign_key.match_type.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support foreign key MATCH clauses",
        ));
    }
    if foreign_key.deferrability.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support deferrable foreign keys",
        ));
    }
    if foreign_key.validation.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support foreign key validation metadata",
        ));
    }
    if foreign_key.enforcement.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support foreign key enforcement metadata",
        ));
    }

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

/// MySQL's [`Dialect`](squealy::Dialect): `?` placeholders, backtick-quoted identifiers, MySQL `CAST`
/// target types, and float division (so `/` needs no float cast). The shared core renderer
/// ([`squealy::render`]) drives MySQL query rendering through this.
// Wired up by the MySQL query objects (the next step), which render through `squealy::render`.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MysqlDialect;

impl squealy::Dialect for MysqlDialect {
    fn write_placeholder(&self, _index: usize, writer: &mut dyn Write) -> io::Result<()> {
        // MySQL placeholders are positional `?`, unnumbered.
        writer.write_all(b"?")
    }

    fn write_quoted_ident(&self, ident: &str, mut writer: &mut dyn Write) -> io::Result<()> {
        write_quoted_ident(ident, &mut writer)
    }

    fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
        // `CAST(expr AS <type>)` accepts a restricted vocabulary in MySQL, distinct from column types
        // (e.g. `SIGNED`/`UNSIGNED`/`CHAR`, not `INT`/`VARCHAR`).
        let name = match ty {
            SqlType::Bool
            | SqlType::I8
            | SqlType::I16
            | SqlType::I32
            | SqlType::I64
            | SqlType::I128
            | SqlType::Isize => "SIGNED",
            SqlType::U8
            | SqlType::U16
            | SqlType::U32
            | SqlType::U64
            | SqlType::U128
            | SqlType::Usize => "UNSIGNED",
            SqlType::F32 | SqlType::F64 | SqlType::Decimal { .. } => "DECIMAL",
            SqlType::Date => "DATE",
            SqlType::Time { .. } => "TIME",
            SqlType::Timestamp { .. } => "DATETIME",
            SqlType::Bytes => "BINARY",
            _ => "CHAR",
        };
        writer.write_all(name.as_bytes())
    }

    fn integer_division_needs_float_cast(&self) -> bool {
        // MySQL `/` is always floating-point division; `DIV` is the integer form.
        false
    }

    fn write_limit_offset(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
        writer: &mut dyn Write,
    ) -> io::Result<()> {
        // MySQL accepts OFFSET only as part of a LIMIT clause, so an offset-without-limit query needs
        // a sentinel limit (the documented `18446744073709551615` "all rows" value).
        match (limit, offset) {
            (Some(limit), Some(offset)) => write!(writer, " LIMIT {limit} OFFSET {offset}"),
            (Some(limit), None) => write!(writer, " LIMIT {limit}"),
            (None, Some(offset)) => write!(writer, " LIMIT 18446744073709551615 OFFSET {offset}"),
            (None, None) => Ok(()),
        }
    }

    fn write_default_row_insert(&self, writer: &mut dyn Write) -> io::Result<()> {
        // MySQL's empty-row insert form; `DEFAULT VALUES` is PostgreSQL-only.
        writer.write_all(b" () VALUES ()")
    }
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
        writer.write_all(&value.as_bytes()[start..index])?;
        writer.write_all(delim)?;
        writer.write_all(delim)?;
        start = index + delimiter.len_utf8();
    }
    writer.write_all(&value.as_bytes()[start..])?;
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

/// Renders a `CREATE TABLE` (plus any secondary indexes) for a query-builder [`Table`], used by the
/// `to::<T>()` create path. This is the query-side counterpart to [`write_database`], which renders
/// from the owned [`DatabaseModel`]; here the source is the derived `Table` trait.
///
/// Foreign keys are emitted as table-level `FOREIGN KEY` clauses rather than inline column
/// `REFERENCES`, which MySQL parses but silently does not enforce.
pub(crate) fn write_table(table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"CREATE TABLE ")?;
    write_qualified_name(table.schema_name(), table.name(), writer)?;
    writer.write_all(b" (")?;
    for (index, column) in table.columns().iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(column.name(), writer)?;
        writer.write_all(b" ")?;
        write_mysql_sql_type(&column.column_type().into(), writer)?;
        if !column.nullable() {
            writer.write_all(b" NOT NULL")?;
        }
        if column.auto_increment() {
            writer.write_all(b" AUTO_INCREMENT")?;
        }
        if column.primary_key() {
            writer.write_all(b" PRIMARY KEY")?;
        }
        if let Some(default) = column.default() {
            writer.write_all(b" DEFAULT ")?;
            write_column_default(default, writer)?;
        }
    }
    for column in table.columns() {
        if let Some(reference) = column.references() {
            writer.write_all(b", FOREIGN KEY (")?;
            write_quoted_ident(column.name(), writer)?;
            writer.write_all(b") REFERENCES ")?;
            write_qualified_name(reference.schema_name(), reference.table(), writer)?;
            writer.write_all(b" (")?;
            write_quoted_ident(reference.column(), writer)?;
            writer.write_all(b")")?;
            if let Some(on_delete) = reference.on_delete() {
                write!(writer, " ON DELETE {on_delete}")?;
            }
            if let Some(on_update) = reference.on_update() {
                write!(writer, " ON UPDATE {on_update}")?;
            }
        }
    }
    writer.write_all(b")")?;

    for (position, index) in table.indexes().iter().enumerate() {
        let unique = if index.unique() { "UNIQUE " } else { "" };
        write!(writer, "\nCREATE {unique}INDEX ")?;
        match index.name() {
            Some(name) => write_quoted_ident(name, writer)?,
            None => write_quoted_ident(&derived_index_name(table, *index, position), writer)?,
        }
        writer.write_all(b" ON ")?;
        write_qualified_name(table.schema_name(), table.name(), writer)?;
        writer.write_all(b" (")?;
        write_quoted_idents(index.columns(), writer)?;
        writer.write_all(b")")?;
    }

    Ok(())
}

/// Builds a deterministic, unique name for an index that did not supply one, so two unnamed indexes
/// on a table do not collide.
fn derived_index_name(
    table: &(dyn Table + Sync),
    index: &dyn squealy::Index,
    position: usize,
) -> String {
    let mut name = format!("idx_{}", table.name());
    for column in index.columns() {
        name.push('_');
        name.push_str(column);
    }
    if index.columns().is_empty() {
        name.push_str(&format!("_{position}"));
    }
    name
}

fn write_quoted_idents(values: &[&'static str], writer: &mut impl Write) -> io::Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(value, writer)?;
    }
    Ok(())
}

fn write_column_default(default: ColumnDefault, writer: &mut impl Write) -> io::Result<()> {
    match default {
        ColumnDefault::Null => writer.write_all(b"NULL"),
        ColumnDefault::Int(value) => write!(writer, "{value}"),
        ColumnDefault::UInt(value) => write!(writer, "{value}"),
        ColumnDefault::Float(value) => write!(writer, "{value}"),
        ColumnDefault::Text(value) => write_quoted_text(value, writer),
        ColumnDefault::Bool(true) => writer.write_all(b"TRUE"),
        ColumnDefault::Bool(false) => writer.write_all(b"FALSE"),
        ColumnDefault::CurrentTimestamp => writer.write_all(b"CURRENT_TIMESTAMP"),
        ColumnDefault::CurrentDate => writer.write_all(b"(CURRENT_DATE)"),
        ColumnDefault::CurrentTime => writer.write_all(b"(CURRENT_TIME)"),
        ColumnDefault::Raw(value) => writer.write_all(value.as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use squealy::Dialect;

    fn dialect_cast(ty: SqlType) -> String {
        let mut out = Vec::new();
        MysqlDialect.write_cast_type(&ty, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn mysql_dialect_renders_its_seams() {
        let mut placeholder = Vec::new();
        MysqlDialect.write_placeholder(3, &mut placeholder).unwrap();
        assert_eq!(placeholder, b"?", "MySQL placeholders are positional `?`");

        let mut ident = Vec::new();
        MysqlDialect
            .write_quoted_ident("user`s", &mut ident)
            .unwrap();
        assert_eq!(String::from_utf8(ident).unwrap(), "`user``s`");

        // CAST target types differ from column types.
        assert_eq!(dialect_cast(SqlType::I32), "SIGNED");
        assert_eq!(dialect_cast(SqlType::U64), "UNSIGNED");
        assert_eq!(dialect_cast(SqlType::F64), "DECIMAL");
        assert_eq!(dialect_cast(SqlType::String), "CHAR");

        assert!(
            !MysqlDialect.integer_division_needs_float_cast(),
            "MySQL `/` is already float division"
        );
    }

    fn limit_offset(limit: Option<usize>, offset: Option<usize>) -> String {
        let mut out = Vec::new();
        MysqlDialect
            .write_limit_offset(limit, offset, &mut out)
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn mysql_offset_without_limit_gets_a_sentinel_limit() {
        // MySQL rejects a bare OFFSET, so an offset-only query needs a max LIMIT.
        assert_eq!(
            limit_offset(None, Some(5)),
            " LIMIT 18446744073709551615 OFFSET 5"
        );
        assert_eq!(limit_offset(Some(10), Some(5)), " LIMIT 10 OFFSET 5");
        assert_eq!(limit_offset(Some(10), None), " LIMIT 10");
        assert_eq!(limit_offset(None, None), "");
    }

    #[test]
    fn mysql_default_row_insert_uses_empty_values() {
        let mut out = Vec::new();
        MysqlDialect.write_default_row_insert(&mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), " () VALUES ()");
    }

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
