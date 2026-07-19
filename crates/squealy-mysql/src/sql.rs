//! MySQL DDL rendering for an owned [`DatabaseModel`].
//!
//! Structurally parallel to the PostgreSQL renderer, differing only in dialect: backtick identifier
//! quoting, MySQL type names (incl. unsigned integers), `AUTO_INCREMENT` identity, and `VARCHAR`-backed
//! strings. The parallelism is itself a finding — a future refactor could share a dialect-parameterized
//! writer — but each backend keeps its own renderer for now.

use std::io::{self, Write};

use squealy::{
    CheckModel, ColumnDefault, ColumnModel, Constraint, ConstraintEnforcement, DatabaseModel,
    DatabasePlan, DatabasePlanStep, DefaultValue, ForeignKeyModel, GeneratedStorage, IndexModel,
    IndexPrefixLength, SqlType, Table, TableModel, TablePlanStep,
};

/// Renders ordered create-from-scratch DDL for a whole model. Statements are `;`-terminated and
/// newline-separated: namespaces, then tables (with inline PK/unique/check), then indexes, then
/// foreign keys as separate `ALTER TABLE … ADD CONSTRAINT`.
pub(crate) fn write_database(model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
    let mut first = true;

    // MySQL has no standalone `CREATE TYPE`. A model declaring an enum type is rejected up front (even an
    // unused one), so a direct `render_create` call cannot silently omit it. An enum *column* is also
    // rejected when rendered, but this guard covers a schema that declares only the type.
    if let Some(enum_type) = model.schemas.iter().flat_map(|schema| &schema.enums).next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "MySQL does not support the user-defined enum type `{}`",
                enum_type.name
            ),
        ));
    }

    // MySQL has no standalone sequence object; reject a model that declares one up front.
    if let Some(sequence) = model
        .schemas
        .iter()
        .flat_map(|schema| &schema.sequences)
        .next()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("MySQL does not support the sequence `{}`", sequence.name),
        ));
    }

    // MySQL has no domain object; reject a model that declares one up front.
    if let Some(domain) = model
        .schemas
        .iter()
        .flat_map(|schema| &schema.domains)
        .next()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("MySQL does not support the domain `{}`", domain.name),
        ));
    }

    // MySQL has no exclusion constraint; reject a model that declares one up front.
    if let Some(exclusion) = model
        .schemas
        .iter()
        .flat_map(|schema| &schema.tables)
        .flat_map(|table| &table.exclusions)
        .next()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "MySQL does not support the exclusion constraint `{}`",
                exclusion.name
            ),
        ));
    }

    // MySQL has no materialized views; reject a model that declares one up front.
    if let Some(view) = model
        .schemas
        .iter()
        .flat_map(|schema| &schema.views)
        .find(|view| view.materialized)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "MySQL does not support the materialized view `{}`",
                view.name
            ),
        ));
    }

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

    // Views are created last (all tables exist) and in dependency order so a view that selects from
    // another view follows it.
    for (schema_name, view) in squealy::ordered_views(model) {
        statement(writer, &mut first)?;
        squealy::render_create_view(schema_name, view, false, &MysqlDialect, writer)?;
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
        DatabasePlanStep::CreateView { schema, view } => {
            statement(writer, first)?;
            // MySQL has no materialized views. Reject on the incremental path (the create path rejects up
            // front / via capabilities), mirroring the enum/sequence/domain handling.
            if view.materialized {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "MySQL does not support the materialized view `{}`",
                        view.name
                    ),
                ));
            }
            // `CREATE OR REPLACE VIEW` so a body change re-runs cleanly; a column-set change is
            // preceded by a `DropView` from the diff.
            squealy::render_create_view(schema.as_deref(), view, true, &MysqlDialect, writer)?;
        }
        DatabasePlanStep::DropView { schema, view } => {
            statement(writer, first)?;
            // MySQL has no materialized views (rejected up front), so this is always a plain `DROP VIEW`.
            squealy::render_drop_view(
                schema.as_deref(),
                &view.name,
                view.materialized,
                &MysqlDialect,
                writer,
            )?;
        }
        // MySQL has no standalone `CREATE TYPE` object. The create path rejects an enum model via
        // capabilities; the incremental plan path skips that check, so reject here too.
        DatabasePlanStep::CreateEnum { enum_type, .. }
        | DatabasePlanStep::DropEnum { enum_type, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "MySQL does not support the user-defined enum type `{}`",
                    enum_type.name
                ),
            ));
        }
        DatabasePlanStep::AlterEnum { after, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "MySQL does not support the user-defined enum type `{}`",
                    after.name
                ),
            ));
        }
        // MySQL has no standalone sequence object. Reject on the incremental path (the create path
        // rejects via capabilities), mirroring the enum handling above.
        DatabasePlanStep::CreateSequence { sequence, .. }
        | DatabasePlanStep::DropSequence { sequence, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MySQL does not support the sequence `{}`", sequence.name),
            ));
        }
        DatabasePlanStep::AlterSequence { after, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MySQL does not support the sequence `{}`", after.name),
            ));
        }
        DatabasePlanStep::SetSequenceOwner { name, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MySQL does not support the sequence `{name}`"),
            ));
        }
        // MySQL has no domain object. Reject on the incremental path, mirroring the enum/sequence handling.
        DatabasePlanStep::CreateDomain { domain, .. }
        | DatabasePlanStep::DropDomain { domain, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MySQL does not support the domain `{}`", domain.name),
            ));
        }
        DatabasePlanStep::AlterDomain { after, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MySQL does not support the domain `{}`", after.name),
            ));
        }
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
            // MySQL `BINARY(N)` is a native type with no generated constraint to rename alongside.
            column_type: _,
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
            write_named_constraint("PRIMARY KEY", constraint, writer)?;
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
            write_named_constraint("UNIQUE", constraint, writer)?;
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
            write_named_constraint("PRIMARY KEY", after, writer)?;
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
            write_named_constraint("UNIQUE", after, writer)?;
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
            if before.name == after.name
                && before.expression == after.expression
                && after.validation.is_none()
            {
                // Only the enforcement changed. Toggle it in place with `ALTER CHECK`, which is atomic:
                // enabling enforcement on a check whose rows violate it fails and LEAVES the check
                // intact. The DROP + ADD below would instead commit the DROP (MySQL auto-commits DDL)
                // and then fail the re-add's validation, silently losing the constraint. The
                // `after.validation.is_none()` guard is required: MySQL has no validation metadata, so a
                // desired check carrying it must reach `write_check` (via DROP + ADD) to be rejected —
                // the incremental plan path skips `validate_capabilities`, so this fast path would
                // otherwise silently emit an enforcement toggle and re-plan the ignored validation
                // forever.
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema, table, writer)?;
                writer.write_all(b" ALTER CHECK ")?;
                write_quoted_ident(&after.name, writer)?;
                write_alter_check_enforcement(&after.enforcement, writer)?;
            } else {
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
        }
        TablePlanStep::AlterIndex { before, after } => {
            writer.write_all(b"DROP INDEX ")?;
            write_quoted_ident(&before.name, writer)?;
            writer.write_all(b" ON ")?;
            write_qualified_name(schema, table, writer)?;
            statement(writer, first)?;
            write_create_index(schema, table, after, writer)?;
        }
        // MySQL has no exclusion constraint. Reject on the incremental path (the create path rejects via
        // capabilities), mirroring the enum/sequence/domain handling.
        TablePlanStep::AddExclusion { exclusion } | TablePlanStep::DropExclusion { exclusion } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "MySQL does not support the exclusion constraint `{}`",
                    exclusion.name
                ),
            ));
        }
        TablePlanStep::AlterExclusion { after, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "MySQL does not support the exclusion constraint `{}`",
                    after.name
                ),
            ));
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
        write_named_constraint("PRIMARY KEY", primary_key, writer)?;
    }
    for unique in &table.uniques {
        entry(writer, &mut first_entry)?;
        write_named_constraint("UNIQUE", unique, writer)?;
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
        write_default_value(default, &column.ty, writer)?;
    }
    if let Some(reason) = column.on_update_shape_error() {
        // A malformed `on_update` (a non-`CURRENT_TIMESTAMP` node, a non-temporal column, or a generated
        // column) would render invalid MySQL DDL, which might fail only at execution after earlier
        // auto-committed statements — reject it here (and in the capability preflight) instead.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("MySQL {reason} (column `{}`)", column.name),
        ));
    }
    if column.on_update.is_some() {
        write_on_update(&column.ty, writer)?;
    }
    if column.identity.is_some() {
        writer.write_all(b" AUTO_INCREMENT")?;
    }
    if let Some(generated) = &column.generated {
        let expression = require_generated_expression(&column.name, &generated.expression)?;
        writer.write_all(b" GENERATED ALWAYS AS (")?;
        squealy::render_scalar_expr(expression, &MysqlDialect, writer)?;
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

/// Rejects a generated column with no expression instead of emitting invalid
/// `GENERATED ALWAYS AS ()`. The `#[column(generated)]` derive attribute marks a column as generated
/// but has no way to supply the expression, so such a model cannot be rendered.
fn require_generated_expression<'a>(
    column: &str,
    expression: &'a Option<squealy::ExprNode>,
) -> io::Result<&'a squealy::ExprNode> {
    match expression {
        Some(expr) if !is_blank_raw(expr) => Ok(expr),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "generated column `{column}` has no expression; #[column(generated)] cannot supply \
                 one, so it cannot be rendered to DDL"
            ),
        )),
    }
}

/// Whether an expression is a verbatim [`squealy::ExprNode::Raw`] holding only whitespace — an empty
/// generated expression that would render as invalid `GENERATED ALWAYS AS ()`.
fn is_blank_raw(expr: &squealy::ExprNode) -> bool {
    matches!(expr, squealy::ExprNode::Raw(sql) if sql.trim().is_empty())
}

/// Renders the neutral [`SqlType`] as a MySQL DDL type.
///
/// MySQL differs from PostgreSQL in several ways the neutral model surfaces: native unsigned integers,
/// a `TINYINT(1)` boolean, no unbounded `text` usable in keys (so bare `String` becomes `VARCHAR(255)`),
/// no native `uuid` (rendered `CHAR(36)`) and only `JSON` (so `Jsonb` also renders `JSON`).
fn write_mysql_sql_type(ty: &SqlType, writer: &mut impl Write) -> io::Result<()> {
    let name = match ty {
        // MySQL has native inline `ENUM(...)` on a column, but no standalone `CREATE TYPE` object, so a
        // model built around a named enum type cannot round-trip here. Reject rather than mis-render.
        SqlType::Enum(name) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MySQL does not support the user-defined enum type `{name}`"),
            ));
        }
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
        // `TIME(n)`/`TIMESTAMP(n)`/`DATETIME(n)` when the model carries a fractional-seconds precision;
        // a bare form (fsp 0) when it does not. `TIMESTAMP` is timezone-aware, `DATETIME` is not.
        SqlType::Time { precision, .. } => return write_mysql_temporal(writer, "TIME", *precision),
        SqlType::Timestamp {
            tz: true,
            precision,
        } => return write_mysql_temporal(writer, "TIMESTAMP", *precision),
        SqlType::Timestamp {
            tz: false,
            precision,
        } => return write_mysql_temporal(writer, "DATETIME", *precision),
        SqlType::Uuid => "CHAR(36)",
        SqlType::Json | SqlType::Jsonb => "JSON",
        SqlType::Bytes => "BLOB",
        // Fixed-width binary: MySQL has a native `BINARY(N)` type (the width round-trips directly),
        // but it caps at 255 bytes — a larger `[u8; N]` has no fixed-width representation, so fail
        // rather than emit DDL the server rejects.
        SqlType::FixedBytes(length) => {
            if *length > 255 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("MySQL BINARY supports at most 255 bytes, but the column is {length}"),
                ));
            }
            return write!(writer, "BINARY({length})");
        }
        SqlType::Raw(raw) => raw.as_str(),
    };
    writer.write_all(name.as_bytes())
}

/// Renders a `TIME`/`TIMESTAMP`/`DATETIME` type with its optional fractional-seconds precision.
fn write_mysql_temporal(
    writer: &mut dyn Write,
    base: &str,
    precision: Option<u8>,
) -> io::Result<()> {
    writer.write_all(base.as_bytes())?;
    if let Some(precision) = precision {
        write!(writer, "({precision})")?;
    }
    Ok(())
}

/// The fractional-seconds precision to attach to a `CURRENT_TIMESTAMP` default so it matches its
/// column: MySQL rejects (or truncates) a `CURRENT_TIMESTAMP`/`CURRENT_TIME` default on a `TIMESTAMP(n)`
/// / `TIME(n)` column unless the function is spelled with the matching `(n)`. Only a non-zero precision
/// needs the suffix (fsp 0 takes the bare form).
fn current_temporal_precision(ty: &SqlType) -> Option<u8> {
    match ty {
        SqlType::Timestamp {
            precision: Some(precision),
            ..
        }
        | SqlType::Time {
            precision: Some(precision),
            ..
        } if *precision > 0 => Some(*precision),
        _ => None,
    }
}

fn write_default_value(
    default: &DefaultValue,
    column_ty: &SqlType,
    writer: &mut impl Write,
) -> io::Result<()> {
    match default {
        DefaultValue::Null => writer.write_all(b"NULL"),
        DefaultValue::Int(value) => write!(writer, "{value}"),
        DefaultValue::UInt(value) => write!(writer, "{value}"),
        DefaultValue::Float(value) => write!(writer, "{value}"),
        DefaultValue::Text(value) => write_quoted_text(value, writer),
        DefaultValue::Bool(true) => writer.write_all(b"TRUE"),
        DefaultValue::Bool(false) => writer.write_all(b"FALSE"),
        DefaultValue::CurrentTimestamp => {
            writer.write_all(b"CURRENT_TIMESTAMP")?;
            if let Some(precision) = current_temporal_precision(column_ty) {
                write!(writer, "({precision})")?;
            }
            Ok(())
        }
        DefaultValue::CurrentDate => writer.write_all(b"(CURRENT_DATE)"),
        DefaultValue::CurrentTime => {
            writer.write_all(b"(CURRENT_TIME")?;
            if let Some(precision) = current_temporal_precision(column_ty) {
                write!(writer, "({precision})")?;
            }
            writer.write_all(b")")
        }
        DefaultValue::Raw(value) => writer.write_all(value.as_bytes()),
    }
}

/// Renders a column's `ON UPDATE CURRENT_TIMESTAMP` auto-update clause. The value is validated by
/// [`ColumnModel::on_update_shape_error`] before this is called (only `CURRENT_TIMESTAMP` on a
/// `TIMESTAMP`/`DATETIME` non-generated column is representable), so this only renders. MySQL forces the
/// clause's fractional-seconds precision to equal the column's own, so the fsp is taken from the column
/// type (like [`write_default_value`]'s `CurrentTimestamp` arm), not from the node.
fn write_on_update(column_ty: &SqlType, writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b" ON UPDATE CURRENT_TIMESTAMP")?;
    if let Some(precision) = current_temporal_precision(column_ty) {
        write!(writer, "({precision})")?;
    }
    Ok(())
}

fn write_named_constraint(
    kind: &str,
    constraint: &Constraint,
    writer: &mut impl Write,
) -> io::Result<()> {
    // A `UNIQUE`/`PRIMARY KEY` over a leading column prefix renders `(col(n))`, like a prefix index.
    validate_prefix_lengths(
        &format!("constraint `{}`", constraint.name),
        constraint.columns.len(),
        &constraint.prefix_lengths,
    )?;
    writer.write_all(b"CONSTRAINT ")?;
    write_quoted_ident(&constraint.name, writer)?;
    write!(writer, " {kind} (")?;
    write_constraint_columns(constraint, writer)?;
    writer.write_all(b")")
}

/// Writes a constraint's key columns, injecting `(n)` after a column indexed over only a leading prefix
/// (MySQL `col(n)`). Mirrors [`write_index_columns`] for the constraint model.
fn write_constraint_columns(constraint: &Constraint, writer: &mut impl Write) -> io::Result<()> {
    for (position, column) in constraint.columns.iter().enumerate() {
        if position > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(column, writer)?;
        if let Some(prefix) = constraint
            .prefix_lengths
            .iter()
            .find(|prefix| prefix.position == position)
        {
            write!(writer, "({})", prefix.length)?;
        }
    }
    Ok(())
}

/// Validates a sparse list of column prefix lengths (an index's or a constraint's) before rendering,
/// surfacing the shared [`squealy::prefix_length_shape_error`] as an `io::Error`. `owner` is a
/// pre-formatted label for the message (e.g. `` index `foo` `` / `` constraint `bar` ``). The capability
/// preflight runs the same check, but the incremental plan render path skips it, so each renderer
/// validates here too.
fn validate_prefix_lengths(
    owner: &str,
    num_columns: usize,
    prefix_lengths: &[IndexPrefixLength],
) -> io::Result<()> {
    match squealy::prefix_length_shape_error(num_columns, prefix_lengths) {
        Some(reason) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{owner} {reason}"),
        )),
        None => Ok(()),
    }
}

fn write_check(check: &CheckModel, writer: &mut impl Write) -> io::Result<()> {
    if check.validation.is_some() {
        // MySQL has no `NOT VALID` (deferred validation) — that is a PostgreSQL concept.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MySQL does not support constraint validation metadata",
        ));
    }
    writer.write_all(b"CONSTRAINT ")?;
    write_quoted_ident(&check.name, writer)?;
    writer.write_all(b" CHECK (")?;
    squealy::render_scalar_expr(&check.expression, &MysqlDialect, writer)?;
    writer.write_all(b")")?;
    write_constraint_enforcement(&check.enforcement, writer)
}

/// Renders a check constraint's `[NOT] ENFORCED` clause in a `CHECK (...)` definition (MySQL 8.0.16+).
/// The default, `ENFORCED`, renders bare so a plain check round-trips (introspection folds the enforced
/// default to `None`, mirroring how PostgreSQL renders `validation`); only `NOT ENFORCED` is a
/// meaningful, rendered difference.
fn write_constraint_enforcement(
    enforcement: &Option<ConstraintEnforcement>,
    writer: &mut impl Write,
) -> io::Result<()> {
    match enforcement {
        Some(ConstraintEnforcement::NotEnforced) => writer.write_all(b" NOT ENFORCED")?,
        Some(ConstraintEnforcement::Raw(enforcement)) => {
            writer.write_all(b" ")?;
            writer.write_all(enforcement.as_bytes())?;
        }
        Some(ConstraintEnforcement::Enforced) | None => {}
    }
    Ok(())
}

/// Renders the enforcement keyword for an in-place `ALTER TABLE ... ALTER CHECK <name> ...` toggle. Unlike
/// the `CHECK (...)` clause above, `ALTER CHECK` requires an explicit keyword, so the enforced default
/// renders `ENFORCED` rather than bare.
fn write_alter_check_enforcement(
    enforcement: &Option<ConstraintEnforcement>,
    writer: &mut impl Write,
) -> io::Result<()> {
    match enforcement {
        Some(ConstraintEnforcement::NotEnforced) => writer.write_all(b" NOT ENFORCED"),
        Some(ConstraintEnforcement::Raw(enforcement)) => {
            writer.write_all(b" ")?;
            writer.write_all(enforcement.as_bytes())
        }
        Some(ConstraintEnforcement::Enforced) | None => writer.write_all(b" ENFORCED"),
    }
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
    if !index.prefix_lengths.is_empty() {
        validate_prefix_lengths(
            &format!("index `{}`", index.name),
            index.columns.len(),
            &index.prefix_lengths,
        )?;
        // A unique index renders `CREATE UNIQUE INDEX`, which MySQL exposes as a `UNIQUE` row in
        // `information_schema.TABLE_CONSTRAINTS`; introspection reconstructs it as a neutral unique
        // `Constraint` (which now carries prefix lengths — see `write_named_constraint`), NOT an
        // `IndexModel`. So a unique *index* with a prefix cannot round-trip as an index — it would
        // replan as a constraint. Reject it and steer the caller to a `#[unique]`/`UNIQUE` constraint,
        // which does round-trip the prefix (git-bug 1847e75).
        if index.unique {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "MySQL cannot round-trip a unique index with column prefix lengths \
                     (index `{}`): it introspects back as a unique constraint, not an index — \
                     use a unique constraint to carry the prefix",
                    index.name
                ),
            ));
        }
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
        // A prefix index keys only a leading `length`-character/byte prefix of the column: `col(length)`.
        if let Some(prefix) = index
            .prefix_lengths
            .iter()
            .find(|prefix| prefix.position == position)
        {
            write!(writer, "({})", prefix.length)?;
        }
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

    /// MySQL has no `UPDATE … FROM`; a correlated update joins the source before `SET`
    /// (`UPDATE t JOIN other ON … SET …`).
    fn update_from_style(&self) -> squealy::UpdateFromStyle {
        squealy::UpdateFromStyle::MysqlJoin
    }

    // --- Upsert (`INSERT … ON DUPLICATE KEY UPDATE`) ---

    /// MySQL references the proposed row as `VALUES(\`col\`)` (vs PostgreSQL's `EXCLUDED."col"`). The
    /// 8.0.19+ row-alias form (`AS new … new.col`) is a follow-up; `VALUES()` is the widely-compatible
    /// spelling.
    fn write_excluded_column(&self, column: &str, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(b"VALUES(")?;
        self.write_quoted_ident(column, writer)?;
        writer.write_all(b")")
    }

    /// MySQL has no conflict target — `ON DUPLICATE KEY UPDATE` matches on every PK/UNIQUE key — so the
    /// target is ignored and this is just the keyword that introduces the assignment list.
    fn write_upsert_set_prefix(&self, _target: &[&str], writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(b" ON DUPLICATE KEY UPDATE ")
    }

    /// MySQL has no `DO NOTHING`; emulate it by self-assigning a column (a no-op update). Prefer an
    /// inserted column; fall back to a conflict-target column for a column-less (`DEFAULT VALUES`)
    /// insert, which has no inserted column to assign. The conflict target always has at least one
    /// column (it comes from `on_conflict(|t| …)`), so the no-op clause is never silently dropped.
    fn write_upsert_do_nothing(
        &self,
        target: &[&str],
        first_column: Option<&str>,
        writer: &mut dyn Write,
    ) -> io::Result<()> {
        if let Some(column) = first_column.or_else(|| target.first().copied()) {
            writer.write_all(b" ON DUPLICATE KEY UPDATE ")?;
            self.write_quoted_ident(column, writer)?;
            writer.write_all(b" = ")?;
            self.write_quoted_ident(column, writer)?;
        }
        Ok(())
    }

    fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
        // A user-defined enum is a PostgreSQL-only type; MySQL has no equivalent CAST target, and the
        // `_ => "CHAR"` fall-through below would silently rewrite the cast's semantics. Reject it (the
        // enum column class is already refused up front — this catches an enum buried in an expression).
        if let SqlType::Enum(name) = ty {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("MySQL cannot render a CAST to the user-defined enum type `{name}`"),
            ));
        }
        // `CAST(expr AS <type>)` accepts a restricted vocabulary in MySQL, distinct from column types
        // (e.g. `SIGNED`/`UNSIGNED`/`CHAR`, not `INT`/`VARCHAR`).
        let name = match ty {
            // 128-bit ints exceed MySQL's 64-bit `SIGNED`/`UNSIGNED`, so cast to a full-precision
            // decimal (e.g. a widened `SUM(BIGINT UNSIGNED)`) rather than overflowing.
            SqlType::I128 | SqlType::U128 => "DECIMAL(65, 0)",
            SqlType::Bool
            | SqlType::I8
            | SqlType::I16
            | SqlType::I32
            | SqlType::I64
            | SqlType::Isize => "SIGNED",
            SqlType::U8 | SqlType::U16 | SqlType::U32 | SqlType::U64 | SqlType::Usize => "UNSIGNED",
            // `CAST(x AS DECIMAL)` with no scale is `DECIMAL(10, 0)` and truncates the fraction, so
            // float results (e.g. `AVG`) cast to `DOUBLE` to stay fractional.
            SqlType::F32 | SqlType::F64 => "DOUBLE",
            SqlType::Decimal { .. } => "DECIMAL",
            SqlType::Date => "DATE",
            // A timestamp/time cast carries its fractional-seconds precision (`DATETIME(6)`), so a
            // `CASE`/`COALESCE` result feeding a `TIMESTAMP(6)` column keeps its microseconds rather than
            // being truncated to fsp 0. (`TIMESTAMP` is not a valid MySQL cast target — `DATETIME` is.)
            SqlType::Time { precision, .. } => {
                return write_mysql_temporal(writer, "TIME", *precision);
            }
            SqlType::Timestamp { precision, .. } => {
                return write_mysql_temporal(writer, "DATETIME", *precision);
            }
            // Both variable and fixed-width binary cast to `BINARY` so a binary expression operand in
            // `CASE`/`NULLIF`/`COALESCE` stays binary instead of being coerced through the text charset.
            SqlType::Bytes | SqlType::FixedBytes(_) => "BINARY",
            _ => "CHAR",
        };
        writer.write_all(name.as_bytes())
    }

    fn write_general_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
        squealy::reject_128bit_general_cast(ty)?;
        // A general authored cast spells the precision/scale faithfully — `CAST(x AS DECIMAL(10, 2))` —
        // unlike a result-pin cast (bare `DECIMAL`, whose exact type is recovered from the output column).
        // MySQL's `CAST` accepts `DECIMAL(M, D)`, so a cross-dialect-deployed decimal cast keeps its scale
        // and round-trips (the reverse parser's `general_cast` now structures MySQL `Decimal` casts). 8fe1530.
        if let SqlType::Decimal { precision, scale } = ty {
            // MySQL's DECIMAL is limited to 1 <= precision <= 65, scale <= 30, and scale <= precision. A
            // general cast outside that range (e.g. a PostgreSQL `numeric(100, 50)` in a cross-dialect
            // package, or a hand-built zero-precision decimal) has no faithful MySQL rendering — reject it at
            // the fidelity boundary rather than emit DDL that errors only at execution.
            if *precision == 0 || *precision > 65 || *scale > 30 || scale > precision {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "MySQL cannot render a general CAST to DECIMAL({precision}, {scale}) \
                         (DECIMAL is limited to 1 <= precision <= 65, scale <= 30, and scale <= precision)"
                    ),
                ));
            }
            return write!(writer, "DECIMAL({precision}, {scale})");
        }
        self.write_cast_type(ty, writer)
    }

    fn integer_division_needs_float_cast(&self) -> bool {
        // MySQL `/` is always floating-point division; `DIV` is the integer form.
        false
    }

    fn now_fractional_digits(&self) -> Option<u8> {
        // MySQL's bare `CURRENT_TIMESTAMP` is fsp 0; the microsecond `now()` value types feed
        // `TIMESTAMP(6)` columns, so render `CURRENT_TIMESTAMP(6)` to keep the sub-seconds.
        Some(6)
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

    fn write_order_nulls(
        &self,
        _nulls: squealy::OrderNulls,
        _writer: &mut dyn Write,
    ) -> io::Result<()> {
        // MySQL has no `NULLS FIRST`/`NULLS LAST` modifier, so a view carrying one drops it here
        // rather than emitting syntax MySQL rejects. (Query-builder `ORDER BY` instead emulates it via
        // `emulates_order_nulls` below, which the view-DDL path does not use.)
        Ok(())
    }

    fn emulates_order_nulls(&self) -> bool {
        // MySQL lacks `NULLS FIRST/LAST`; the renderer emits a leading `(<expr> IS NULL)` sort key.
        true
    }

    fn write_row_lock(&self, lock: squealy::RowLock, writer: &mut dyn Write) -> io::Result<()> {
        // MySQL spells the shared lock `LOCK IN SHARE MODE` (no `FOR SHARE` keyword).
        writer.write_all(match lock {
            squealy::RowLock::Update => b" FOR UPDATE",
            squealy::RowLock::Share => b" LOCK IN SHARE MODE",
        })
    }

    fn extract_second_uses_microsecond_unit(&self) -> bool {
        // MySQL's `EXTRACT(SECOND …)` is integer-only; use the composite `SECOND_MICROSECOND` unit to
        // recover the fractional part.
        true
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
        let ty: SqlType = column.column_type().into();
        write_mysql_sql_type(&ty, writer)?;
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
            write_column_default(default, &ty, writer)?;
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
    // A single-column `#[column(unique, where = ...)]` is a partial unique index, carried on
    // `Column::unique_predicate()` (not in `table.uniques()`). MySQL has no partial indexes, so it
    // is rejected here rather than silently dropped, mirroring the table-level case below.
    for column in table.columns() {
        if column.unique_predicate().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MySQL does not support partial (filtered) unique indexes",
            ));
        }
    }
    // Table-level `#[unique(columns = [..])]` constraints render as trailing constraints too, so the
    // direct `write_table` path matches the model-based renderer and actually enforces uniqueness.
    // MySQL has no partial indexes, so a `where = ...` predicate is rejected rather than silently
    // dropped (mirrors the model-based `write_create_index`).
    for unique in table.uniques() {
        if unique.predicate.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MySQL does not support partial (filtered) unique indexes",
            ));
        }
        writer.write_all(b", ")?;
        if let Some(name) = unique.name {
            writer.write_all(b"CONSTRAINT ")?;
            write_quoted_ident(name, writer)?;
            writer.write_all(b" ")?;
        }
        writer.write_all(b"UNIQUE (")?;
        write_quoted_idents(unique.columns, writer)?;
        writer.write_all(b")")?;
    }
    writer.write_all(b")")?;

    for (position, index) in table.indexes().iter().enumerate() {
        if index.predicate().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MySQL does not support partial index predicates",
            ));
        }
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

fn write_column_default(
    default: ColumnDefault,
    column_ty: &SqlType,
    writer: &mut impl Write,
) -> io::Result<()> {
    match default {
        ColumnDefault::Null => writer.write_all(b"NULL"),
        ColumnDefault::Int(value) => write!(writer, "{value}"),
        ColumnDefault::UInt(value) => write!(writer, "{value}"),
        ColumnDefault::Float(value) => write!(writer, "{value}"),
        ColumnDefault::Text(value) => write_quoted_text(value, writer),
        ColumnDefault::Bool(true) => writer.write_all(b"TRUE"),
        ColumnDefault::Bool(false) => writer.write_all(b"FALSE"),
        // Match the column's precision so MySQL accepts the default (see `write_default_value`).
        ColumnDefault::CurrentTimestamp => {
            writer.write_all(b"CURRENT_TIMESTAMP")?;
            if let Some(precision) = current_temporal_precision(column_ty) {
                write!(writer, "({precision})")?;
            }
            Ok(())
        }
        ColumnDefault::CurrentDate => writer.write_all(b"(CURRENT_DATE)"),
        ColumnDefault::CurrentTime => {
            writer.write_all(b"(CURRENT_TIME")?;
            if let Some(precision) = current_temporal_precision(column_ty) {
                write!(writer, "({precision})")?;
            }
            writer.write_all(b")")
        }
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
        // Floats cast to `DOUBLE` (fractional), and 128-bit ints to a full-precision decimal.
        assert_eq!(dialect_cast(SqlType::F64), "DOUBLE");
        assert_eq!(dialect_cast(SqlType::I128), "DECIMAL(65, 0)");
        assert_eq!(dialect_cast(SqlType::U128), "DECIMAL(65, 0)");
        assert_eq!(dialect_cast(SqlType::String), "CHAR");
        // A timestamp/time cast carries its precision (`TIMESTAMP` is not a MySQL cast target;
        // `DATETIME` is), so a `CASE`/`COALESCE` result keeps its microseconds.
        assert_eq!(
            dialect_cast(SqlType::Timestamp {
                tz: true,
                precision: Some(6)
            }),
            "DATETIME(6)"
        );
        assert_eq!(
            dialect_cast(SqlType::Time {
                tz: false,
                precision: None
            }),
            "TIME"
        );

        assert!(
            !MysqlDialect.integer_division_needs_float_cast(),
            "MySQL `/` is already float division"
        );
    }

    #[test]
    fn mysql_rejects_a_cast_to_an_enum_type() {
        // An enum column is refused up front, but an enum buried in a cast expression reaches the cast
        // renderer, where the `_ => "CHAR"` fall-through would silently rewrite its semantics.
        let enum_ty = SqlType::Enum("mood".to_owned());
        let result = MysqlDialect.write_cast_type(&enum_ty, &mut Vec::new());
        let error = result.expect_err("MySQL must reject a CAST to an enum type");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("mood"), "{error}");

        // The general (authored) cast path delegates to the same rejection.
        let result = MysqlDialect.write_general_cast_type(&enum_ty, &mut Vec::new());
        assert!(
            result.is_err(),
            "general cast to an enum must also be rejected"
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
    fn mysql_rejects_generated_column_without_expression() {
        use squealy::{
            ColumnModel, DatabaseModel, GeneratedColumnModel, GeneratedStorage, SchemaModel,
            TableModel,
        };

        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
                enums: Vec::new(),
                sequences: Vec::new(),
                domains: Vec::new(),
                tables: vec![TableModel {
                    name: "people".to_owned(),
                    comment: None,
                    columns: vec![ColumnModel {
                        name: "full_name".to_owned(),
                        comment: None,
                        ty: SqlType::String,
                        collation: None,
                        nullable: true,
                        default: None,
                        identity: None,
                        generated: Some(GeneratedColumnModel {
                            expression: None,
                            storage: GeneratedStorage::Stored,
                        }),
                        on_update: None,
                    }],
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                    exclusions: Vec::new(),
                }],
            }],
        };

        let mut out = Vec::new();
        let error = write_database(&model, &mut out).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("full_name"), "{error}");
    }

    #[test]
    fn mysql_check_enforcement_renders_only_the_non_default() {
        let check = |enforcement| CheckModel {
            name: "c_pos".to_owned(),
            expression: squealy::ExprNode::Compare {
                op: squealy::CompareOp::GreaterThan,
                left: Box::new(squealy::ExprNode::BareColumn {
                    column: "n".to_owned(),
                }),
                right: Box::new(squealy::ExprNode::Literal("0".to_owned())),
            },
            validation: None,
            enforcement,
        };
        let render = |enforcement| {
            let mut out = Vec::new();
            write_check(&check(enforcement), &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };

        // NOT ENFORCED is the only meaningful, rendered difference; the enforced default (and None)
        // render bare so a plain check round-trips against the introspected form.
        assert_eq!(
            render(Some(ConstraintEnforcement::NotEnforced)),
            "CONSTRAINT `c_pos` CHECK ((`n` > 0)) NOT ENFORCED"
        );
        assert_eq!(
            render(Some(ConstraintEnforcement::Enforced)),
            "CONSTRAINT `c_pos` CHECK ((`n` > 0))"
        );
        assert_eq!(render(None), "CONSTRAINT `c_pos` CHECK ((`n` > 0))");
        assert_eq!(
            render(Some(ConstraintEnforcement::Raw("NOT ENFORCED".to_owned()))),
            "CONSTRAINT `c_pos` CHECK ((`n` > 0)) NOT ENFORCED"
        );
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

    fn default_sql(default: DefaultValue, ty: SqlType) -> String {
        let mut out = Vec::new();
        write_default_value(&default, &ty, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn mysql_current_temporal_defaults_carry_precision() {
        // A `CURRENT_TIMESTAMP`/`CURRENT_TIME` default matches its column's fractional precision so
        // MySQL accepts it and the value keeps its microseconds; fsp 0 / `None` take the bare form.
        let ts6 = SqlType::Timestamp {
            tz: true,
            precision: Some(6),
        };
        assert_eq!(
            default_sql(DefaultValue::CurrentTimestamp, ts6),
            "CURRENT_TIMESTAMP(6)"
        );
        let time3 = SqlType::Time {
            tz: false,
            precision: Some(3),
        };
        assert_eq!(
            default_sql(DefaultValue::CurrentTime, time3),
            "(CURRENT_TIME(3))"
        );
        // fsp 0 and unspecified precision take the bare spelling.
        let ts0 = SqlType::Timestamp {
            tz: true,
            precision: Some(0),
        };
        assert_eq!(
            default_sql(DefaultValue::CurrentTimestamp, ts0),
            "CURRENT_TIMESTAMP"
        );
        let time_none = SqlType::Time {
            tz: false,
            precision: None,
        };
        assert_eq!(
            default_sql(DefaultValue::CurrentTime, time_none),
            "(CURRENT_TIME)"
        );
    }

    fn on_update_sql(ty: SqlType) -> String {
        let mut out = Vec::new();
        write_on_update(&ty, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn mysql_on_update_current_timestamp_carries_column_precision() {
        // The `ON UPDATE CURRENT_TIMESTAMP` fsp is forced to equal the column's own precision, so it is
        // derived from the column type (like a `CURRENT_TIMESTAMP` default), not from the node.
        assert_eq!(
            on_update_sql(SqlType::Timestamp {
                tz: false,
                precision: Some(6),
            }),
            " ON UPDATE CURRENT_TIMESTAMP(6)"
        );
        // fsp 0 and an unspecified precision take the bare spelling.
        assert_eq!(
            on_update_sql(SqlType::Timestamp {
                tz: false,
                precision: Some(0),
            }),
            " ON UPDATE CURRENT_TIMESTAMP"
        );
        assert_eq!(
            on_update_sql(SqlType::Timestamp {
                tz: false,
                precision: None,
            }),
            " ON UPDATE CURRENT_TIMESTAMP"
        );
    }

    #[test]
    fn mysql_column_renders_default_then_on_update() {
        // The canonical MySQL idiom `updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE
        // CURRENT_TIMESTAMP` — the `ON UPDATE` clause follows the `DEFAULT`, both matching the column fsp.
        use squealy::ColumnModel;
        let column = ColumnModel {
            name: "updated_at".to_owned(),
            comment: None,
            ty: SqlType::Timestamp {
                tz: true,
                precision: Some(3),
            },
            collation: None,
            nullable: false,
            default: Some(DefaultValue::CurrentTimestamp),
            identity: None,
            generated: None,
            on_update: Some(Box::new(squealy::ExprNode::Now)),
        };
        let mut out = Vec::new();
        write_column(&column, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "`updated_at` TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3)"
        );
    }

    #[test]
    fn mysql_write_column_rejects_a_malformed_on_update() {
        // The shape invariant lives on `ColumnModel::on_update_shape_error` (unit-tested in squealy-ir);
        // `write_column` must enforce it so a malformed hand-authored package is rejected during
        // rendering rather than left to fail at DDL-execution time.
        use squealy::ColumnModel;
        let temporal = SqlType::Timestamp {
            tz: true,
            precision: None,
        };
        let malformed = [
            // A non-`CURRENT_TIMESTAMP` node.
            ColumnModel {
                ty: temporal.clone(),
                on_update: Some(Box::new(squealy::ExprNode::Raw(
                    "now() + interval 1 day".to_owned(),
                ))),
                ..plain_column("ts")
            },
            // A non-temporal column type.
            ColumnModel {
                ty: SqlType::I32,
                on_update: Some(Box::new(squealy::ExprNode::Now)),
                ..plain_column("ts")
            },
            // A generated column.
            ColumnModel {
                ty: temporal,
                on_update: Some(Box::new(squealy::ExprNode::Now)),
                generated: Some(squealy::GeneratedColumnModel {
                    expression: Some(squealy::ExprNode::Now),
                    storage: GeneratedStorage::Virtual,
                }),
                ..plain_column("ts")
            },
        ];
        for column in malformed {
            let mut out = Vec::new();
            let error = write_column(&column, &mut out).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("ts"), "{error}");
        }
    }

    fn plain_column(name: &str) -> squealy::ColumnModel {
        squealy::ColumnModel {
            name: name.to_owned(),
            comment: None,
            ty: SqlType::Timestamp {
                tz: true,
                precision: None,
            },
            collation: None,
            nullable: false,
            default: None,
            identity: None,
            generated: None,
            on_update: None,
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
        assert_eq!(
            render_type(SqlType::Time {
                tz: false,
                precision: None
            }),
            "TIME"
        );
        assert_eq!(
            render_type(SqlType::Timestamp {
                tz: false,
                precision: None
            }),
            "DATETIME"
        );
        assert_eq!(
            render_type(SqlType::Timestamp {
                tz: true,
                precision: None
            }),
            "TIMESTAMP"
        );
        // A fractional-seconds precision renders `TIMESTAMP(n)`/`DATETIME(n)`/`TIME(n)`.
        assert_eq!(
            render_type(SqlType::Timestamp {
                tz: true,
                precision: Some(6)
            }),
            "TIMESTAMP(6)"
        );
        assert_eq!(
            render_type(SqlType::Timestamp {
                tz: false,
                precision: Some(3)
            }),
            "DATETIME(3)"
        );
        assert_eq!(
            render_type(SqlType::Time {
                tz: false,
                precision: Some(6)
            }),
            "TIME(6)"
        );
        assert_eq!(render_type(SqlType::Uuid), "CHAR(36)");
        assert_eq!(render_type(SqlType::Json), "JSON");
        assert_eq!(render_type(SqlType::Jsonb), "JSON");
        assert_eq!(render_type(SqlType::Bytes), "BLOB");
    }
}
