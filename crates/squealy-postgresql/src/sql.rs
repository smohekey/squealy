use std::io::{self, Write};

use squealy::{Column, ColumnDefault, Index, SqlType, Table};

/// Constraint-name prefix for the generated fixed-width-binary length check. Placed at the *start* so
/// introspection can recognize the generated check and distinguish it from a user-authored
/// `octet_length` check.
pub(crate) const FIXED_BYTES_CHECK_PREFIX: &str = "sqfb_";

/// Deterministic constraint name for a `FixedBytes` column's length check, so create, alter-column,
/// and introspection all agree on the same name (and a width change can drop/re-add it).
///
/// The name is `sqfb_` plus a stable 64-bit hash of the *column* name:
/// - fixed length (21 bytes), so it never hits PostgreSQL's 63-byte identifier truncation — and two
///   columns can't truncate to the same name (the round-2 collision finding);
/// - derived from the column only (not the table), so it survives a table rename. A column *rename*
///   does change it, so the `RenameColumn` step renames the constraint too (PostgreSQL does not do so
///   automatically), keeping create/alter/introspection agreed on the name.
pub(crate) fn fixed_bytes_check_name(column: &str) -> String {
    format!("{FIXED_BYTES_CHECK_PREFIX}{:016x}", fnv1a64(column))
}

/// FNV-1a (64-bit). A small, dependency-free hash that is stable across runs and compiler versions —
/// required because the generated constraint name must match between the create and a later alter.
fn fnv1a64(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Renders a `FixedBytes` column's inline width check: `CONSTRAINT "sqfb_<hash>" CHECK
/// (octet_length("col") = N)`.
fn write_fixed_bytes_check(column: &str, width: u32, writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b" CONSTRAINT ")?;
    write_quoted_ident(&fixed_bytes_check_name(column), writer)?;
    writer.write_all(b" CHECK (octet_length(")?;
    write_quoted_ident(column, writer)?;
    write!(writer, ") = {width})")
}

/// PostgreSQL's [`Dialect`]: positional `$n` placeholders, `"`-quoted identifiers, and `double
/// precision` casts. The query renderer routes its dialect-specific output through this so the sink
/// logic can be shared (see [`squealy::Dialect`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PostgresDialect;

impl squealy::Dialect for PostgresDialect {
    fn write_placeholder(&self, index: usize, writer: &mut dyn Write) -> io::Result<()> {
        // PostgreSQL parameters are 1-based and positional.
        write!(writer, "${}", index + 1)
    }

    fn write_quoted_ident(&self, ident: &str, mut writer: &mut dyn Write) -> io::Result<()> {
        write_quoted_ident(ident, &mut writer)
    }

    fn write_cast_type(&self, ty: &SqlType, mut writer: &mut dyn Write) -> io::Result<()> {
        if let SqlType::Enum(name) = ty {
            // A cast to an enum type inside an expression (a check/generated/index/view body) has no
            // schema context here, so it would render an unqualified `AS "mood"` that fails to resolve
            // when the enum's schema is off `search_path`. Reject rather than emit unresolvable SQL.
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "squealy does not support casting to the enum type `{name}` in an expression"
                ),
            ));
        }
        write_pg_sql_type(ty, &mut writer)
    }

    fn write_like_operator(
        &self,
        case_insensitive: bool,
        negated: bool,
        writer: &mut dyn Write,
    ) -> io::Result<()> {
        // PostgreSQL has a native case-insensitive `ILIKE`.
        writer.write_all(match (case_insensitive, negated) {
            (false, false) => b" LIKE " as &[u8],
            (false, true) => b" NOT LIKE ",
            (true, false) => b" ILIKE ",
            (true, true) => b" NOT ILIKE ",
        })
    }

    fn concat_uses_pipe_operator(&self) -> bool {
        // `a || b` propagates NULL (matching the builder's nullability model) and lets a bare
        // parameter's type be inferred, unlike PostgreSQL's NULL-ignoring `CONCAT("any", …)`.
        true
    }

    fn substring_bounds_need_cast(&self) -> bool {
        // Cast `start`/`len` to integer so a bare parameter is the positional count, not the regex
        // `substring(text FROM pattern FOR escape)` overload.
        true
    }

    fn timestamp_operand_needs_cast(&self) -> bool {
        // Cast a bare literal/param operand of EXTRACT/date_trunc to its timestamp type — both are
        // overloaded, so an untyped placeholder can't be resolved when preparing the statement.
        true
    }
}

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
        write_column_type(*column, writer)?;
        if column.primary_key() {
            writer.write_all(b" PRIMARY KEY")?;
        }
        if column.auto_increment() {
            writer.write_all(b" GENERATED BY DEFAULT AS IDENTITY")?;
        }
        if !column.nullable() {
            writer.write_all(b" NOT NULL")?;
        }
        if let Some(default) = column.default() {
            writer.write_all(b" DEFAULT ")?;
            write_default(default, writer)?;
        }
        if let Some(reference) = column.references() {
            writer.write_all(b" REFERENCES ")?;
            write_qualified_name(reference.schema_name(), reference.table(), writer)?;
            writer.write_all(b"(")?;
            write_quoted_ident(reference.column(), writer)?;
            writer.write_all(b")")?;
            if let Some(on_delete) = reference.on_delete() {
                write!(writer, " ON DELETE {on_delete}")?;
            }
            if let Some(on_update) = reference.on_update() {
                write!(writer, " ON UPDATE {on_update}")?;
            }
        }
        // Fixed-width binary: enforce the byte width with a named inline `octet_length` CHECK
        // (PostgreSQL has no native fixed-length binary type). Introspection folds the named check
        // back to `FixedBytes(N)`.
        if let squealy::ColumnType::FixedBytes(width) = column.column_type() {
            write_fixed_bytes_check(column.name(), width, writer)?;
        }
    }
    // A table-level `#[primary_key(columns = [..])]` is not hung off any single column, so render it
    // as a trailing constraint here (the per-column `primary_key` form above covers single columns).
    if let Some(primary_key) = table.primary_key() {
        writer.write_all(b", ")?;
        if let Some(name) = primary_key.name {
            writer.write_all(b"CONSTRAINT ")?;
            write_quoted_ident(name, writer)?;
            writer.write_all(b" ")?;
        }
        writer.write_all(b"PRIMARY KEY (")?;
        write_quoted_idents(primary_key.columns, writer)?;
        writer.write_all(b")")?;
    }
    // Table-level `#[unique(columns = [..])]` constraints render as trailing constraints too, so the
    // direct `write_table` path matches the model-based renderer and actually enforces uniqueness.
    // A unique carrying a `where = ...` predicate is *not* a table constraint (Postgres cannot put a
    // `WHERE` on one); it is emitted as a partial unique index after the table instead.
    for unique in table.uniques() {
        if unique.predicate.is_some() {
            continue;
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
        if let Some(predicate) = index.predicate() {
            writer.write_all(b" WHERE ")?;
            squealy::render_scalar_expr(&predicate(), &PostgresDialect, writer)?;
        }
    }

    // Predicated uniques, emitted as partial unique indexes so the `WHERE` is honored: first the
    // single-column `#[column(unique, where = ...)]` form (carried on `Column::unique_predicate`,
    // not in `table.uniques()`), then the table-level `#[unique(columns = [..], where = ...)]` form.
    // Both use the `uq_<table>_<columns>` name that matches the model path.
    for column in table.columns() {
        let Some(predicate) = column.unique_predicate() else {
            continue;
        };
        let name = derived_unique_name(table, &[column.name()]);
        writer.write_all(b"\nCREATE UNIQUE INDEX ")?;
        write_quoted_ident(&name, writer)?;
        writer.write_all(b" ON ")?;
        write_qualified_name(table.schema_name(), table.name(), writer)?;
        writer.write_all(b" (")?;
        write_quoted_ident(column.name(), writer)?;
        writer.write_all(b") WHERE ")?;
        squealy::render_scalar_expr(&predicate(), &PostgresDialect, writer)?;
    }
    for unique in table.uniques() {
        let Some(predicate) = unique.predicate else {
            continue;
        };
        writer.write_all(b"\nCREATE UNIQUE INDEX ")?;
        match unique.name {
            Some(name) => write_quoted_ident(name, writer)?,
            None => write_quoted_ident(&derived_unique_name(table, unique.columns), writer)?,
        }
        writer.write_all(b" ON ")?;
        write_qualified_name(table.schema_name(), table.name(), writer)?;
        writer.write_all(b" (")?;
        write_quoted_idents(unique.columns, writer)?;
        writer.write_all(b") WHERE ")?;
        squealy::render_scalar_expr(&predicate(), &PostgresDialect, writer)?;
    }

    Ok(())
}

/// Builds the `uq_<table>_<columns>` name a partial unique index falls back to when the
/// `#[unique(..)]` declaration did not supply an explicit name. Mirrors the model builder's
/// `uq_name` convention so the two DDL paths agree.
fn derived_unique_name(table: &(dyn Table + Sync), columns: &[&str]) -> String {
    let mut name = format!("uq_{}", table.name());
    for column in columns {
        name.push('_');
        name.push_str(column);
    }
    name
}

/// Builds a deterministic, unique index name for an index that did not supply one.
/// Without this, every unnamed index would render as the same name and collide.
fn derived_index_name(table: &(dyn Table + Sync), index: &dyn Index, position: usize) -> String {
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

fn write_column_type(column: &dyn Column, writer: &mut impl Write) -> io::Result<()> {
    write_pg_sql_type(&column.column_type().into(), writer)
}

/// Whole-database DDL rendering, used by the `SchemaBackend` impl. Gated behind the `schema` feature
/// so query-only users carry none of it.
#[cfg(feature = "schema")]
pub(crate) mod ddl {
    use std::io::{self, Write};

    use squealy::{
        CheckModel, ColumnModel, Constraint, ConstraintEnforcement, ConstraintValidation,
        DatabaseModel, DatabasePlan, DatabasePlanStep, DefaultValue, DomainModel, EnumModel,
        ForeignKeyModel, IdentityMode, IndexModel, SequenceModel, SqlType, TableModel,
        TablePlanStep,
    };

    use super::{write_pg_sql_type, write_qualified_name, write_quoted_ident, write_quoted_text};

    /// Renders ordered create-from-scratch DDL for a whole [`DatabaseModel`].
    ///
    /// Statements are emitted in phases so creation never depends on ordering: namespaces, then tables
    /// (with primary-key/unique/check constraints inline), then indexes, then foreign keys as separate
    /// `ALTER TABLE … ADD CONSTRAINT`. Statements are `;`-terminated and newline-separated.
    pub(crate) fn write_database(model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
        let mut first = true;

        for schema in &model.schemas {
            if let Some(name) = schema.name.as_deref() {
                statement(writer, &mut first)?;
                writer.write_all(b"CREATE SCHEMA IF NOT EXISTS ")?;
                write_quoted_ident(name, writer)?;
            }
        }

        // Enum types are created before any table, since a table column can be of an enum type.
        for schema in &model.schemas {
            for enum_type in &schema.enums {
                statement(writer, &mut first)?;
                write_create_enum(schema.name.as_deref(), enum_type, writer)?;
            }
        }

        // Domains are created before any table, since a table column can be of a domain type.
        for schema in &model.schemas {
            for domain in &schema.domains {
                statement(writer, &mut first)?;
                write_create_domain(schema.name.as_deref(), domain, writer)?;
            }
        }

        // Sequences are created (without their `OWNED BY`) before any table, since a column default can
        // `nextval` a sequence. The `OWNED BY` clause is applied after the tables exist, below.
        for schema in &model.schemas {
            for sequence in &schema.sequences {
                statement(writer, &mut first)?;
                write_create_sequence(schema.name.as_deref(), sequence, writer)?;
            }
        }

        for schema in &model.schemas {
            for table in &schema.tables {
                statement(writer, &mut first)?;
                write_create_table(schema.name.as_deref(), table, writer)?;
            }
        }

        // Now that every table exists, tie each owned sequence to its column.
        for schema in &model.schemas {
            for sequence in &schema.sequences {
                if sequence.owned_by.is_some() {
                    statement(writer, &mut first)?;
                    writer.write_all(b"ALTER SEQUENCE ")?;
                    write_qualified_name(schema.name.as_deref(), &sequence.name, writer)?;
                    write_sequence_owned_by(
                        schema.name.as_deref(),
                        sequence.owned_by.as_ref(),
                        writer,
                    )?;
                }
            }
        }

        for schema in &model.schemas {
            for table in &schema.tables {
                if let Some(comment) = &table.comment {
                    statement(writer, &mut first)?;
                    write_comment_on_table(
                        schema.name.as_deref(),
                        &table.name,
                        Some(comment),
                        writer,
                    )?;
                }
                for column in &table.columns {
                    if let Some(comment) = &column.comment {
                        statement(writer, &mut first)?;
                        write_comment_on_column(
                            schema.name.as_deref(),
                            &table.name,
                            &column.name,
                            Some(comment),
                            writer,
                        )?;
                    }
                }
            }
        }

        for schema in &model.schemas {
            for table in &schema.tables {
                for index in &table.indexes {
                    statement(writer, &mut first)?;
                    write_create_index(schema.name.as_deref(), &table.name, index, false, writer)?;
                }
            }
        }

        for schema in &model.schemas {
            for table in &schema.tables {
                for foreign_key in &table.foreign_keys {
                    statement(writer, &mut first)?;
                    write_add_foreign_key(
                        schema.name.as_deref(),
                        &table.name,
                        foreign_key,
                        writer,
                    )?;
                }
            }
        }

        // Views are created last: all tables already exist, and views are emitted in dependency order
        // so a view that selects from another view is created after it.
        for (schema_name, view) in squealy::ordered_views(model) {
            statement(writer, &mut first)?;
            squealy::render_create_view(schema_name, view, false, &super::PostgresDialect, writer)?;
        }

        // Terminate the final statement (the separator only terminates *preceding* ones).
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

    /// Renders index-add steps with `CREATE INDEX CONCURRENTLY`. Statements are `;`-separated so the
    /// caller (`execute_ddl_unmanaged`) can run each one outside a transaction, as `CONCURRENTLY`
    /// requires. Non-index steps fall back to normal rendering (the planner partitions them out).
    pub(crate) fn write_plan_concurrent(
        plan: &DatabasePlan,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        let mut first = true;
        for step in &plan.steps {
            if let DatabasePlanStep::AlterTable {
                schema,
                table,
                change,
            } = step
                && let TablePlanStep::AddIndex { index } = change.as_ref()
            {
                statement(writer, &mut first)?;
                write_create_index(schema.as_deref(), table, index, true, writer)?;
            } else {
                write_plan_step(step, writer, &mut first)?;
            }
        }
        write_deferred_foreign_keys(plan, writer, &mut first)?;
        if !first {
            writer.write_all(b";")?;
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
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema.as_deref(), from, writer)?;
                writer.write_all(b" RENAME TO ")?;
                write_quoted_ident(to, writer)?;
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
                // Incremental view changes use `CREATE OR REPLACE VIEW`, so a body change re-runs
                // cleanly without a drop. A column-set change is preceded by a `DropView` from the diff.
                squealy::render_create_view(
                    schema.as_deref(),
                    view,
                    true,
                    &super::PostgresDialect,
                    writer,
                )?;
            }
            DatabasePlanStep::DropView { schema, view } => {
                statement(writer, first)?;
                squealy::render_drop_view(
                    schema.as_deref(),
                    &view.name,
                    &super::PostgresDialect,
                    writer,
                )?;
            }
            DatabasePlanStep::CreateEnum { schema, enum_type } => {
                statement(writer, first)?;
                write_create_enum(schema.as_deref(), enum_type, writer)?;
            }
            DatabasePlanStep::DropEnum { schema, enum_type } => {
                statement(writer, first)?;
                writer.write_all(b"DROP TYPE ")?;
                write_qualified_name(schema.as_deref(), &enum_type.name, writer)?;
            }
            DatabasePlanStep::AlterEnum { after, .. } => {
                // Changing an enum's labels (append, remove, or reorder) is not supported yet. A correct
                // migration needs live-schema and whole-plan awareness — dropping/restoring live column
                // defaults, rebuilding dependent foreign keys, dropping dependent views, and ordering
                // around table changes and PostgreSQL's `ADD VALUE`-in-transaction rule. That is tracked
                // as its own follow-up; for now, refuse rather than emit SQL PostgreSQL would reject.
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "squealy does not yet support changing the labels of the enum type `{}`; \
                         recreate the type manually or add labels out of band",
                        after.name
                    ),
                ));
            }
            DatabasePlanStep::CreateSequence { schema, sequence } => {
                statement(writer, first)?;
                write_create_sequence(schema.as_deref(), sequence, writer)?;
            }
            DatabasePlanStep::AlterSequence { schema, after, .. } => {
                statement(writer, first)?;
                writer.write_all(b"ALTER SEQUENCE ")?;
                write_qualified_name(schema.as_deref(), &after.name, writer)?;
                write_sequence_attributes(after, writer)?;
            }
            DatabasePlanStep::SetSequenceOwner {
                schema,
                name,
                owned_by,
            } => {
                statement(writer, first)?;
                writer.write_all(b"ALTER SEQUENCE ")?;
                write_qualified_name(schema.as_deref(), name, writer)?;
                write_sequence_owned_by(schema.as_deref(), owned_by.as_ref(), writer)?;
            }
            DatabasePlanStep::DropSequence { schema, sequence } => {
                statement(writer, first)?;
                writer.write_all(b"DROP SEQUENCE ")?;
                write_qualified_name(schema.as_deref(), &sequence.name, writer)?;
            }
            DatabasePlanStep::CreateDomain { schema, domain } => {
                statement(writer, first)?;
                write_create_domain(schema.as_deref(), domain, writer)?;
            }
            DatabasePlanStep::AlterDomain {
                schema,
                before,
                after,
            } => {
                write_alter_domain(schema.as_deref(), before, after, writer, first)?;
            }
            DatabasePlanStep::DropDomain { schema, domain } => {
                statement(writer, first)?;
                writer.write_all(b"DROP DOMAIN ")?;
                write_qualified_name(schema.as_deref(), &domain.name, writer)?;
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
        if let Some(comment) = &table.comment {
            statement(writer, first)?;
            write_comment_on_table(schema, &table.name, Some(comment), writer)?;
        }
        for column in &table.columns {
            if let Some(comment) = &column.comment {
                statement(writer, first)?;
                write_comment_on_column(schema, &table.name, &column.name, Some(comment), writer)?;
            }
        }
        for index in &table.indexes {
            statement(writer, first)?;
            write_create_index(schema, &table.name, index, false, writer)?;
        }
        Ok(())
    }

    /// Emits the `ADD FOREIGN KEY` constraints for every table created in `plan`, deferred until all
    /// `CreateTable` steps have rendered. A single plan can create several tables in name order, so a
    /// foreign key pointing at a later-created table would otherwise reference a table that does not
    /// exist yet; running the constraints in a second pass makes the create order irrelevant.
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
        writer.write_all(b"CREATE SCHEMA IF NOT EXISTS \"__squealy\"")
    }

    fn write_create_refactor_log_table(writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(
            b"CREATE TABLE IF NOT EXISTS \"__squealy\".\"refactors\" (\
\"id\" text PRIMARY KEY, \
\"applied_at\" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        )
    }

    fn write_record_refactor(refactor_id: &str, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(b"INSERT INTO \"__squealy\".\"refactors\" (\"id\") VALUES (")?;
        write_quoted_text(refactor_id, writer)?;
        writer.write_all(b") ON CONFLICT (\"id\") DO NOTHING")
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
                write_comment_on_table(schema, table, after.as_ref(), writer)?;
            }
            TablePlanStep::AddColumn { column } => {
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema, table, writer)?;
                writer.write_all(b" ADD COLUMN ")?;
                write_model_column(schema, column, writer)?;
                if let Some(comment) = &column.comment {
                    statement(writer, first)?;
                    write_comment_on_column(schema, table, &column.name, Some(comment), writer)?;
                }
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
                column_type,
            } => {
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema, table, writer)?;
                writer.write_all(b" RENAME COLUMN ")?;
                write_quoted_ident(from, writer)?;
                writer.write_all(b" TO ")?;
                write_quoted_ident(to, writer)?;
                // The generated `FixedBytes` length check is named from the column, and PostgreSQL does
                // not rename it with the column. Rename it too so it keeps matching the deterministic
                // name (the introspection fold and a later width-change `DROP` both rely on it).
                if matches!(column_type, squealy::SqlType::FixedBytes(_)) {
                    statement(writer, first)?;
                    writer.write_all(b"ALTER TABLE ")?;
                    write_qualified_name(schema, table, writer)?;
                    writer.write_all(b" RENAME CONSTRAINT ")?;
                    write_quoted_ident(&super::fixed_bytes_check_name(from), writer)?;
                    writer.write_all(b" TO ")?;
                    write_quoted_ident(&super::fixed_bytes_check_name(to), writer)?;
                }
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
            TablePlanStep::DropPrimaryKey { constraint }
            | TablePlanStep::DropUnique { constraint } => {
                write_drop_constraint(schema, table, &constraint.name, writer)?;
            }
            TablePlanStep::AddUnique { constraint } => {
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema, table, writer)?;
                writer.write_all(b" ADD ")?;
                write_named_constraint("UNIQUE", constraint, writer)?;
            }
            TablePlanStep::AddForeignKey { foreign_key } => {
                write_add_foreign_key(schema, table, foreign_key, writer)?;
            }
            TablePlanStep::DropForeignKey { foreign_key } => {
                write_drop_constraint(schema, table, &foreign_key.name, writer)?;
            }
            TablePlanStep::AddCheck { check } => {
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema, table, writer)?;
                writer.write_all(b" ADD ")?;
                write_check(check, writer)?;
            }
            TablePlanStep::DropCheck { check } => {
                write_drop_constraint(schema, table, &check.name, writer)?;
            }
            TablePlanStep::AddIndex { index } => {
                write_create_index(schema, table, index, false, writer)?;
            }
            TablePlanStep::DropIndex { index } => {
                writer.write_all(b"DROP INDEX ")?;
                write_qualified_name(schema, &index.name, writer)?;
            }
            TablePlanStep::AlterPrimaryKey { before, after } => {
                write_drop_constraint(schema, table, &before.name, writer)?;
                statement(writer, first)?;
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema, table, writer)?;
                writer.write_all(b" ADD ")?;
                write_named_constraint("PRIMARY KEY", after, writer)?;
            }
            TablePlanStep::AlterUnique { before, after } => {
                write_drop_constraint(schema, table, &before.name, writer)?;
                statement(writer, first)?;
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema, table, writer)?;
                writer.write_all(b" ADD ")?;
                write_named_constraint("UNIQUE", after, writer)?;
            }
            TablePlanStep::AlterForeignKey { before, after } => {
                write_drop_constraint(schema, table, &before.name, writer)?;
                statement(writer, first)?;
                write_add_foreign_key(schema, table, after, writer)?;
            }
            TablePlanStep::AlterCheck { before, after } => {
                write_drop_constraint(schema, table, &before.name, writer)?;
                statement(writer, first)?;
                writer.write_all(b"ALTER TABLE ")?;
                write_qualified_name(schema, table, writer)?;
                writer.write_all(b" ADD ")?;
                write_check(after, writer)?;
            }
            TablePlanStep::AlterIndex { before, after } => {
                writer.write_all(b"DROP INDEX ")?;
                write_qualified_name(schema, &before.name, writer)?;
                statement(writer, first)?;
                write_create_index(schema, table, after, false, writer)?;
            }
            TablePlanStep::AlterColumn {
                before,
                after,
                type_cast,
            } => {
                write_alter_column(
                    schema,
                    table,
                    before,
                    after,
                    type_cast.as_deref(),
                    writer,
                    first,
                )?;
            }
        }
        Ok(())
    }

    fn write_alter_column(
        schema: Option<&str>,
        table: &str,
        before: &ColumnModel,
        after: &ColumnModel,
        type_cast: Option<&str>,
        writer: &mut impl Write,
        first: &mut bool,
    ) -> io::Result<()> {
        if before.name != after.name {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PostgreSQL incremental column rename rendering is not supported yet",
            ));
        }
        // Postgres cannot turn an existing column into a generated column (or change its expression)
        // in place; only dropping the generated-ness is possible. Adding/changing requires a
        // drop-and-recreate, which the planner expresses as separate column steps.
        if generated_requires_recreate(before, after) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PostgreSQL cannot add or change a generated column in place; drop and recreate the \
                 column instead",
            ));
        }
        // `ON UPDATE CURRENT_TIMESTAMP` is a MySQL-only attribute. The incremental ALTER path renders
        // each supported property delta piecemeal and never reaches `write_model_column`, so reject it
        // here too — otherwise a cross-dialect package that changes `on_update` alongside a supported
        // property would apply only the supported change and leave the schema perpetually drifting.
        if after.on_update.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "PostgreSQL does not support an `ON UPDATE` column attribute (column `{}`)",
                    after.name
                ),
            ));
        }

        // Fixed-width binary length check. The byte width lives entirely in the generated named
        // `octet_length` check (the `TYPE bytea` alter below is a no-op for a width change). The stale
        // check must be dropped *before* the `TYPE` change: changing to a non-`bytea` type while
        // `octet_length(col) = N` still exists fails validation. The replacement check is added at the
        // end, after the new type is in place.
        let before_width = matches!(before.ty, squealy::SqlType::FixedBytes(_));
        let after_width = matches!(after.ty, squealy::SqlType::FixedBytes(_));
        let drop_fixed_bytes_check = before_width && before.ty != after.ty;
        let add_fixed_bytes_check = after_width && before.ty != after.ty;

        // PostgreSQL validates an existing column default against the *new* type independently of the
        // `USING` clause (which only converts the stored rows), so a default the target type cannot
        // accept — an enum/`text` conversion in either direction, a cross-schema enum carried as `Raw`,
        // and so on — makes `ALTER COLUMN ... TYPE` fail while the old default is still attached. Rather
        // than enumerate which conversions PostgreSQL happens to accept in place, drop the existing
        // default before *any* type change and let the `SET DEFAULT` below restore the desired default
        // (forced on when the default is otherwise unchanged across the conversion). Restoring always
        // succeeds because the desired default is by definition valid for the desired type.
        let drop_default_before_type_change = before.default.is_some() && before.ty != after.ty;

        let mut wrote = false;
        if drop_fixed_bytes_check {
            write_drop_constraint(
                schema,
                table,
                &super::fixed_bytes_check_name(&after.name),
                writer,
            )?;
            wrote = true;
        }
        if drop_default_before_type_change {
            if wrote {
                statement(writer, first)?;
            }
            write_alter_column_prefix(schema, table, &after.name, writer)?;
            writer.write_all(b" DROP DEFAULT")?;
            wrote = true;
        }
        if before.ty != after.ty || before.collation != after.collation {
            // The check drop above may have already written a statement, so separate from it.
            if wrote {
                statement(writer, first)?;
            }
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ALTER COLUMN ")?;
            write_quoted_ident(&after.name, writer)?;
            writer.write_all(b" TYPE ")?;
            write_model_column_type(schema, &after.ty, writer)?;
            if let Some(collation) = &after.collation {
                writer.write_all(b" COLLATE ")?;
                write_quoted_ident(collation, writer)?;
            }
            // A `cast-column` refactor hint supplies the `USING` expression for a non-trivial type
            // conversion that Postgres cannot perform implicitly.
            if let Some(cast) = type_cast {
                writer.write_all(b" USING ")?;
                writer.write_all(cast.as_bytes())?;
            }
            wrote = true;
        }

        if before.nullable != after.nullable {
            if wrote {
                statement(writer, first)?;
            }
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ALTER COLUMN ")?;
            write_quoted_ident(&after.name, writer)?;
            if after.nullable {
                writer.write_all(b" DROP NOT NULL")?;
            } else {
                writer.write_all(b" SET NOT NULL")?;
            }
            wrote = true;
        }

        // A column cannot be both an identity column and have a default in PostgreSQL, so the drops
        // must precede the adds: e.g. `SET DEFAULT` is rejected while the column is still an identity
        // column, and `ADD … AS IDENTITY` is rejected while it still has a default.
        if before.identity.is_some() && after.identity.is_none() {
            if wrote {
                statement(writer, first)?;
            }
            write_alter_column_prefix(schema, table, &after.name, writer)?;
            writer.write_all(b" DROP IDENTITY IF EXISTS")?;
            wrote = true;
        }
        if before.default.is_some() && after.default.is_none() && !drop_default_before_type_change {
            if wrote {
                statement(writer, first)?;
            }
            write_alter_column_prefix(schema, table, &after.name, writer)?;
            writer.write_all(b" DROP DEFAULT")?;
            wrote = true;
        }
        if let Some(identity) = &after.identity
            && before.identity.as_ref() != Some(identity)
        {
            if wrote {
                statement(writer, first)?;
            }
            write_alter_column_prefix(schema, table, &after.name, writer)?;
            if before.identity.is_none() {
                writer.write_all(b" ADD GENERATED ")?;
                write_pg_identity_mode(&identity.mode, writer)?;
                writer.write_all(b" AS IDENTITY")?;
            } else {
                writer.write_all(b" SET GENERATED ")?;
                write_pg_identity_mode(&identity.mode, writer)?;
            }
            wrote = true;
        }
        if let Some(default) = &after.default
            && (before.default.as_ref() != Some(default) || drop_default_before_type_change)
        {
            if wrote {
                statement(writer, first)?;
            }
            write_alter_column_prefix(schema, table, &after.name, writer)?;
            writer.write_all(b" SET DEFAULT ")?;
            write_default_value(default, writer)?;
            wrote = true;
        }

        // The only generated-column transition Postgres supports in place: dropping it, which keeps
        // the already-computed values as a plain column.
        if before.generated.is_some() && after.generated.is_none() {
            if wrote {
                statement(writer, first)?;
            }
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ALTER COLUMN ")?;
            write_quoted_ident(&after.name, writer)?;
            writer.write_all(b" DROP EXPRESSION IF EXISTS")?;
            wrote = true;
        }

        if before.comment != after.comment {
            if wrote {
                statement(writer, first)?;
            }
            write_comment_on_column(schema, table, &after.name, after.comment.as_ref(), writer)?;
            wrote = true;
        }

        // Add the replacement width check now that the new `bytea` type is in place (the stale check
        // was dropped before the `TYPE` change above).
        if add_fixed_bytes_check && let squealy::SqlType::FixedBytes(width) = after.ty {
            if wrote {
                statement(writer, first)?;
            }
            writer.write_all(b"ALTER TABLE ")?;
            write_qualified_name(schema, table, writer)?;
            writer.write_all(b" ADD")?;
            super::write_fixed_bytes_check(&after.name, width, writer)?;
            wrote = true;
        }

        if !wrote {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PostgreSQL incremental column alteration has no renderable changes",
            ));
        }

        Ok(())
    }

    /// Whether a generated-column change needs a drop-and-recreate (Postgres cannot add a generated
    /// expression to an existing column, nor change one, in place — only drop it).
    fn generated_requires_recreate(before: &ColumnModel, after: &ColumnModel) -> bool {
        match (&before.generated, &after.generated) {
            (_, None) => false,
            (before, after) => before != after,
        }
    }

    fn write_pg_identity_mode(mode: &IdentityMode, writer: &mut impl Write) -> io::Result<()> {
        match mode {
            IdentityMode::Always => writer.write_all(b"ALWAYS"),
            IdentityMode::ByDefault | IdentityMode::AutoIncrement => {
                writer.write_all(b"BY DEFAULT")
            }
        }
    }

    fn write_alter_column_prefix(
        schema: Option<&str>,
        table: &str,
        column: &str,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"ALTER TABLE ")?;
        write_qualified_name(schema, table, writer)?;
        writer.write_all(b" ALTER COLUMN ")?;
        write_quoted_ident(column, writer)
    }

    fn write_drop_constraint(
        schema: Option<&str>,
        table: &str,
        name: &str,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"ALTER TABLE ")?;
        write_qualified_name(schema, table, writer)?;
        writer.write_all(b" DROP CONSTRAINT ")?;
        write_quoted_ident(name, writer)
    }

    /// Terminates the previous statement and starts a new line before every statement after the first,
    /// leaving the caller to write the statement body. The final statement is terminated by the caller.
    fn statement(writer: &mut impl Write, first: &mut bool) -> io::Result<()> {
        if *first {
            *first = false;
        } else {
            writer.write_all(b";\n")?;
        }
        Ok(())
    }

    /// Renders `CREATE TYPE <name> AS ENUM ('a', 'b', ...)`. Labels are single-quoted string literals in
    /// their declared (sort) order.
    fn write_create_enum(
        schema: Option<&str>,
        enum_type: &EnumModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"CREATE TYPE ")?;
        write_qualified_name(schema, &enum_type.name, writer)?;
        writer.write_all(b" AS ENUM (")?;
        for (index, label) in enum_type.labels.iter().enumerate() {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            write_quoted_text(label, writer)?;
        }
        writer.write_all(b")")
    }

    /// Renders `CREATE SEQUENCE <name> <attributes>` — without the `OWNED BY` clause, which is applied
    /// separately once the owning column exists.
    fn write_create_sequence(
        schema: Option<&str>,
        sequence: &SequenceModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"CREATE SEQUENCE ")?;
        write_qualified_name(schema, &sequence.name, writer)?;
        write_sequence_attributes(sequence, writer)
    }

    /// Renders every sequence attribute explicitly (` AS <type> INCREMENT BY … MINVALUE … MAXVALUE …
    /// START WITH … CACHE … [NO ]CYCLE`), so a published sequence re-plans to empty against the concrete
    /// values `pg_sequence` reports. Shared by `CREATE SEQUENCE` and `ALTER SEQUENCE`.
    fn write_sequence_attributes(
        sequence: &SequenceModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        write!(
            writer,
            " AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {}",
            sequence_data_type_sql(sequence.data_type),
            sequence.increment,
            sequence.min,
            sequence.max,
            sequence.start,
            sequence.cache,
        )?;
        writer.write_all(if sequence.cycle {
            b" CYCLE"
        } else {
            b" NO CYCLE"
        })
    }

    /// Renders ` OWNED BY <table>.<column>` (schema-qualified) or ` OWNED BY NONE`.
    fn write_sequence_owned_by(
        schema: Option<&str>,
        owned_by: Option<&squealy::SequenceOwnedBy>,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b" OWNED BY ")?;
        match owned_by {
            Some(owner) => {
                write_qualified_name(schema, &owner.table, writer)?;
                writer.write_all(b".")?;
                write_quoted_ident(&owner.column, writer)
            }
            None => writer.write_all(b"NONE"),
        }
    }

    fn sequence_data_type_sql(data_type: squealy::SequenceDataType) -> &'static str {
        match data_type {
            squealy::SequenceDataType::SmallInt => "smallint",
            squealy::SequenceDataType::Integer => "integer",
            squealy::SequenceDataType::BigInt => "bigint",
        }
    }

    /// Renders `CREATE DOMAIN <name> AS <base_type> [DEFAULT …] [NOT NULL] [CONSTRAINT … CHECK (…)]…`.
    fn write_create_domain(
        schema: Option<&str>,
        domain: &DomainModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"CREATE DOMAIN ")?;
        write_qualified_name(schema, &domain.name, writer)?;
        writer.write_all(b" AS ")?;
        write_pg_sql_type(&domain.base_type, writer)?;
        if let Some(default) = &domain.default {
            writer.write_all(b" DEFAULT ")?;
            write_default_value(default, writer)?;
        }
        if domain.not_null {
            writer.write_all(b" NOT NULL")?;
        }
        for check in &domain.checks {
            writer.write_all(b" ")?;
            write_check(check, writer)?;
        }
        Ok(())
    }

    /// Renders the granular `ALTER DOMAIN` statements for a domain's `NOT NULL` / `DEFAULT` / `CHECK`
    /// changes. A base-type change cannot be done in place — PostgreSQL has no `ALTER DOMAIN … TYPE` — so
    /// it is refused (recreate the domain manually), mirroring the deferred enum-label migration.
    fn write_alter_domain(
        schema: Option<&str>,
        before: &DomainModel,
        after: &DomainModel,
        writer: &mut impl Write,
        first: &mut bool,
    ) -> io::Result<()> {
        if before.base_type != after.base_type {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "squealy does not support changing the base type of the domain `{}`; recreate it \
                     manually",
                    after.name
                ),
            ));
        }
        // Drop each check that is gone or changed before adding the new/changed ones, so a same-named
        // check whose expression changed is dropped and re-added rather than colliding.
        for before_check in &before.checks {
            if !after.checks.iter().any(|c| c == before_check) {
                statement(writer, first)?;
                writer.write_all(b"ALTER DOMAIN ")?;
                write_qualified_name(schema, &after.name, writer)?;
                writer.write_all(b" DROP CONSTRAINT ")?;
                write_quoted_ident(&before_check.name, writer)?;
            }
        }
        if before.not_null != after.not_null {
            statement(writer, first)?;
            writer.write_all(b"ALTER DOMAIN ")?;
            write_qualified_name(schema, &after.name, writer)?;
            writer.write_all(if after.not_null {
                b" SET NOT NULL"
            } else {
                b" DROP NOT NULL"
            })?;
        }
        if before.default != after.default {
            statement(writer, first)?;
            writer.write_all(b"ALTER DOMAIN ")?;
            write_qualified_name(schema, &after.name, writer)?;
            match &after.default {
                Some(default) => {
                    writer.write_all(b" SET DEFAULT ")?;
                    write_default_value(default, writer)?;
                }
                None => writer.write_all(b" DROP DEFAULT")?,
            }
        }
        for after_check in &after.checks {
            if !before.checks.iter().any(|c| c == after_check) {
                statement(writer, first)?;
                writer.write_all(b"ALTER DOMAIN ")?;
                write_qualified_name(schema, &after.name, writer)?;
                writer.write_all(b" ADD ")?;
                write_check(after_check, writer)?;
            }
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
            write_model_column(schema, column, writer)?;
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

        writer.write_all(b"\n)")
    }

    fn write_comment_on_table(
        schema: Option<&str>,
        table: &str,
        comment: Option<&String>,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"COMMENT ON TABLE ")?;
        write_qualified_name(schema, table, writer)?;
        writer.write_all(b" IS ")?;
        match comment {
            Some(comment) => write_quoted_text(comment, writer),
            None => writer.write_all(b"NULL"),
        }
    }

    fn write_comment_on_column(
        schema: Option<&str>,
        table: &str,
        column: &str,
        comment: Option<&String>,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"COMMENT ON COLUMN ")?;
        write_qualified_name(schema, table, writer)?;
        writer.write_all(b".")?;
        write_quoted_ident(column, writer)?;
        writer.write_all(b" IS ")?;
        match comment {
            Some(comment) => write_quoted_text(comment, writer),
            None => writer.write_all(b"NULL"),
        }
    }

    /// Separates entries inside a `CREATE TABLE (...)` list: each entry is on its own indented line.
    fn entry(writer: &mut impl Write, first: &mut bool) -> io::Result<()> {
        if *first {
            *first = false;
            writer.write_all(b"  ")
        } else {
            writer.write_all(b",\n  ")
        }
    }

    /// Writes a model column's type, qualifying a user-enum type with the table's schema. An enum type
    /// lives in a schema and is not on the session `search_path`, so an unqualified reference (`m mood`)
    /// fails to resolve when the table is in a non-`public` schema; render `m "app"."mood"` instead.
    /// All other types render unqualified.
    fn write_model_column_type(
        schema: Option<&str>,
        ty: &SqlType,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        match ty {
            SqlType::Enum(name) => write_qualified_name(schema, name, writer),
            _ => write_pg_sql_type(ty, writer),
        }
    }

    fn write_model_column(
        schema: Option<&str>,
        column: &ColumnModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        write_quoted_ident(&column.name, writer)?;
        writer.write_all(b" ")?;
        write_model_column_type(schema, &column.ty, writer)?;
        if let Some(collation) = &column.collation {
            writer.write_all(b" COLLATE ")?;
            write_quoted_ident(collation, writer)?;
        }
        if let Some(identity) = &column.identity {
            match identity.mode {
                IdentityMode::Always => writer.write_all(b" GENERATED ALWAYS AS IDENTITY")?,
                IdentityMode::ByDefault | IdentityMode::AutoIncrement => {
                    writer.write_all(b" GENERATED BY DEFAULT AS IDENTITY")?
                }
            }
        }
        if let Some(generated) = &column.generated {
            let expression = require_generated_expression(&column.name, &generated.expression)?;
            writer.write_all(b" GENERATED ALWAYS AS (")?;
            squealy::render_scalar_expr(expression, &super::PostgresDialect, writer)?;
            writer.write_all(b") STORED")?;
        }
        if !column.nullable {
            writer.write_all(b" NOT NULL")?;
        }
        if let Some(default) = &column.default {
            writer.write_all(b" DEFAULT ")?;
            write_default_value(default, writer)?;
        }
        if column.on_update.is_some() {
            // `ON UPDATE CURRENT_TIMESTAMP` is a MySQL-only column attribute. The incremental plan
            // render path does not validate capabilities, so reject it here rather than silently
            // dropping it (mirrors how the other backend-specific column features are rejected).
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "PostgreSQL does not support an `ON UPDATE` column attribute (column `{}`)",
                    column.name
                ),
            ));
        }
        // Fixed-width binary has no native PostgreSQL type, so enforce the width with a named inline
        // CHECK on `octet_length`. Introspection folds this named check back into `FixedBytes(N)` (see
        // `introspect`), so it never appears as a standalone check in the model — keeping publish/status
        // idempotent.
        if let squealy::SqlType::FixedBytes(width) = &column.ty {
            super::write_fixed_bytes_check(&column.name, *width, writer)?;
        }
        Ok(())
    }

    /// Renders an owned [`DefaultValue`]. Mirrors [`write_default`] for the compile-time
    /// [`ColumnDefault`].
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
            DefaultValue::CurrentDate => writer.write_all(b"CURRENT_DATE"),
            DefaultValue::CurrentTime => writer.write_all(b"CURRENT_TIME"),
            DefaultValue::Raw(value) => writer.write_all(value.as_bytes()),
        }
    }

    fn write_named_constraint(
        kind: &str,
        constraint: &Constraint,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        // The incremental plan render path skips `validate_capabilities`, so reject a constraint
        // carrying MySQL-only column prefix lengths here too, rather than silently drop the `(n)` and
        // emit a full-column constraint that would never round-trip (mirrors the index-metadata rejects).
        if !constraint.prefix_lengths.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "PostgreSQL does not support constraint column prefix lengths (constraint `{}`)",
                    constraint.name
                ),
            ));
        }
        writer.write_all(b"CONSTRAINT ")?;
        write_quoted_ident(&constraint.name, writer)?;
        write!(writer, " {kind} (")?;
        write_quoted_ident_list(&constraint.columns, writer)?;
        writer.write_all(b")")
    }

    fn write_check(check: &CheckModel, writer: &mut impl Write) -> io::Result<()> {
        reject_constraint_enforcement(&check.enforcement)?;
        writer.write_all(b"CONSTRAINT ")?;
        write_quoted_ident(&check.name, writer)?;
        writer.write_all(b" CHECK (")?;
        squealy::render_scalar_expr(&check.expression, &super::PostgresDialect, writer)?;
        writer.write_all(b")")?;
        write_constraint_validation(&check.validation, writer)
    }

    fn write_create_index(
        schema: Option<&str>,
        table: &str,
        index: &IndexModel,
        concurrent: bool,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        if !index.prefix_lengths.is_empty() {
            // PostgreSQL has no column-prefix indexes (`col(n)`); it uses expression indexes instead.
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PostgreSQL does not support index column prefix lengths",
            ));
        }
        writer.write_all(b"CREATE ")?;
        if index.unique {
            writer.write_all(b"UNIQUE ")?;
        }
        writer.write_all(b"INDEX ")?;
        if concurrent {
            writer.write_all(b"CONCURRENTLY ")?;
        }
        write_quoted_ident(&index.name, writer)?;
        writer.write_all(b" ON ")?;
        write_qualified_name(schema, table, writer)?;
        if let Some(method) = &index.method {
            writer.write_all(b" USING ")?;
            writer.write_all(method.postgres_sql().as_bytes())?;
        }
        writer.write_all(b" (")?;
        write_index_terms(index, writer)?;
        writer.write_all(b")")?;
        if !index.include_columns.is_empty() {
            writer.write_all(b" INCLUDE (")?;
            write_quoted_ident_list(&index.include_columns, writer)?;
            writer.write_all(b")")?;
        }
        if let Some(predicate) = &index.predicate {
            writer.write_all(b" WHERE ")?;
            squealy::render_scalar_expr(predicate, &super::PostgresDialect, writer)?;
        }
        Ok(())
    }

    fn write_add_foreign_key(
        schema: Option<&str>,
        table: &str,
        foreign_key: &ForeignKeyModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        reject_constraint_enforcement(&foreign_key.enforcement)?;
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
        if let Some(match_type) = &foreign_key.match_type {
            write!(writer, " MATCH {}", match_type.as_sql())?;
        }
        if let Some(on_delete) = &foreign_key.on_delete {
            write!(writer, " ON DELETE {}", on_delete.as_sql())?;
        }
        if let Some(on_update) = &foreign_key.on_update {
            write!(writer, " ON UPDATE {}", on_update.as_sql())?;
        }
        if let Some(deferrability) = &foreign_key.deferrability {
            write!(writer, " {}", deferrability.as_sql())?;
        }
        write_constraint_validation(&foreign_key.validation, writer)?;
        Ok(())
    }

    fn write_constraint_validation(
        validation: &Option<ConstraintValidation>,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        match validation {
            Some(ConstraintValidation::NotValidated) => writer.write_all(b" NOT VALID")?,
            Some(ConstraintValidation::Raw(validation)) => {
                writer.write_all(b" ")?;
                writer.write_all(validation.as_bytes())?;
            }
            Some(ConstraintValidation::Validated) | None => {}
        }
        Ok(())
    }

    fn reject_constraint_enforcement(
        enforcement: &Option<ConstraintEnforcement>,
    ) -> io::Result<()> {
        if enforcement.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PostgreSQL constraint enforcement metadata is not supported by squealy yet",
            ));
        }
        Ok(())
    }

    /// Returns a generated column's defining expression, or rejects a column marked generated with no
    /// expression instead of emitting invalid `GENERATED ALWAYS AS ()`. The `#[column(generated)]` derive
    /// attribute marks a column as generated but has no way to supply the expression, so such a model
    /// (expression `None`, or a blank verbatim `Raw`) cannot be rendered.
    fn require_generated_expression<'a>(
        column: &str,
        expression: &'a Option<squealy::ExprNode>,
    ) -> io::Result<&'a squealy::ExprNode> {
        match expression {
            Some(expr) if !is_blank_raw(expr) => Ok(expr),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "generated column \"{column}\" has no expression; #[column(generated)] cannot \
                     supply one, so it cannot be rendered to DDL"
                ),
            )),
        }
    }

    /// Whether an expression is a verbatim [`squealy::ExprNode::Raw`] holding only whitespace — an empty
    /// generated expression that would render as invalid `GENERATED ALWAYS AS ()`.
    fn is_blank_raw(expr: &squealy::ExprNode) -> bool {
        matches!(expr, squealy::ExprNode::Raw(sql) if sql.trim().is_empty())
    }

    /// Like [`write_quoted_idents`] but over owned model strings.
    fn write_quoted_ident_list(columns: &[String], writer: &mut impl Write) -> io::Result<()> {
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            write_quoted_ident(column, writer)?;
        }
        Ok(())
    }

    fn write_index_terms(index: &IndexModel, writer: &mut impl Write) -> io::Result<()> {
        for (position, column) in index.columns.iter().enumerate() {
            if position > 0 {
                writer.write_all(b", ")?;
            }
            write_quoted_ident(column, writer)?;
            write_index_collation(index, position, writer)?;
            write_index_operator_class(index, position, writer)?;
            write_index_direction(index, position, writer)?;
            write_index_nulls(index, position, writer)?;
        }
        for (offset, expression) in index.expressions.iter().enumerate() {
            let position = index.columns.len() + offset;
            if position > 0 {
                writer.write_all(b", ")?;
            }
            // A structural PostgreSQL expression index key term is wrapped in parentheses. A `Raw` term
            // (a legacy package's verbatim expression, or an un-invertible introspected one — which may
            // carry its own decoration such as `COLLATE`, or be a comma-separated `pg_get_expr` list) is
            // emitted verbatim, as its `ExprNode::Raw` contract requires; wrapping it could change its
            // meaning (a comma list becomes a row constructor).
            match expression {
                squealy::ExprNode::Raw(raw) => writer.write_all(raw.as_bytes())?,
                structural => {
                    writer.write_all(b"(")?;
                    squealy::render_scalar_expr(structural, &super::PostgresDialect, writer)?;
                    writer.write_all(b")")?;
                }
            }
            write_index_collation(index, position, writer)?;
            write_index_operator_class(index, position, writer)?;
            write_index_direction(index, position, writer)?;
            write_index_nulls(index, position, writer)?;
        }
        Ok(())
    }

    fn write_index_direction(
        index: &IndexModel,
        position: usize,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        match index.directions.get(position) {
            Some(squealy::IndexDirection::Asc) => writer.write_all(b" ASC")?,
            Some(squealy::IndexDirection::Desc) => writer.write_all(b" DESC")?,
            None => {}
        }
        Ok(())
    }

    fn write_index_operator_class(
        index: &IndexModel,
        position: usize,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        if let Some(operator_class) = index
            .operator_classes
            .iter()
            .find(|operator_class| operator_class.position == position)
        {
            writer.write_all(b" ")?;
            writer.write_all(operator_class.name.as_bytes())?;
        }
        Ok(())
    }

    fn write_index_collation(
        index: &IndexModel,
        position: usize,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        if let Some(collation) = index
            .collations
            .iter()
            .find(|collation| collation.position == position)
        {
            writer.write_all(b" COLLATE ")?;
            write_quoted_ident(&collation.name, writer)?;
        }
        Ok(())
    }

    fn write_index_nulls(
        index: &IndexModel,
        position: usize,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        match index.nulls.get(position) {
            Some(squealy::IndexNullsOrder::First) => writer.write_all(b" NULLS FIRST")?,
            Some(squealy::IndexNullsOrder::Last) => writer.write_all(b" NULLS LAST")?,
            None => {}
        }
        Ok(())
    }
} // mod ddl

/// Renders the neutral [`SqlType`] as a PostgreSQL DDL type. Used by both the whole-database renderer
/// (via the model) and `write_table` (which converts its compile-time `ColumnType`).
fn write_pg_sql_type(ty: &SqlType, writer: &mut impl Write) -> io::Result<()> {
    let name = match ty {
        SqlType::Bool => "boolean",
        SqlType::I8 | SqlType::I16 => "smallint",
        SqlType::I32 => "integer",
        SqlType::I64 | SqlType::Isize => "bigint",
        SqlType::I128 => "numeric",
        SqlType::U8 => "smallint",
        SqlType::U16 => "integer",
        SqlType::U32 | SqlType::Usize => "bigint",
        SqlType::U64 | SqlType::U128 => "numeric",
        SqlType::F32 => "real",
        SqlType::F64 => "double precision",
        SqlType::String | SqlType::Text => "text",
        SqlType::Varchar(length) => return write!(writer, "varchar({length})"),
        SqlType::Char(length) => return write!(writer, "char({length})"),
        SqlType::Decimal { precision, scale } => {
            return write!(writer, "numeric({precision},{scale})");
        }
        SqlType::Date => "date",
        // `time(n)` / `timestamp(n) [with time zone]` — the fractional-seconds precision goes between the
        // base name and the `with time zone` suffix. `None` renders the bare form (PostgreSQL then uses
        // its microsecond default, which introspection reads back as `Some(6)`).
        SqlType::Time { tz, precision } => {
            return write_pg_temporal(writer, "time", *tz, *precision);
        }
        SqlType::Timestamp { tz, precision } => {
            return write_pg_temporal(writer, "timestamp", *tz, *precision);
        }
        SqlType::Uuid => "uuid",
        SqlType::Json => "json",
        SqlType::Jsonb => "jsonb",
        SqlType::Bytes => "bytea",
        // PostgreSQL has no fixed-length binary type; the width is enforced by a generated
        // `CHECK (octet_length(col) = N)` constraint (see the column-check lowering).
        SqlType::FixedBytes(_) => "bytea",
        // A user-defined enum type is referenced by its (quoted) name; the `CREATE TYPE` itself is a
        // separate schema object rendered before the tables that use it.
        SqlType::Enum(name) => return write_quoted_ident(name, writer),
        SqlType::Raw(raw) => raw.as_str(),
    };
    writer.write_all(name.as_bytes())
}

/// Renders a `time`/`timestamp` type with its optional fractional-seconds precision and the
/// `with time zone` suffix. PostgreSQL spells the precision *inside* the base name (`timestamp(3) with
/// time zone`), so it cannot be a trailing modifier.
fn write_pg_temporal(
    writer: &mut impl Write,
    base: &str,
    tz: bool,
    precision: Option<u8>,
) -> io::Result<()> {
    writer.write_all(base.as_bytes())?;
    if let Some(precision) = precision {
        write!(writer, "({precision})")?;
    }
    if tz {
        writer.write_all(b" with time zone")?;
    }
    Ok(())
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

fn write_default(default: ColumnDefault, writer: &mut impl Write) -> io::Result<()> {
    match default {
        ColumnDefault::Null => writer.write_all(b"NULL"),
        ColumnDefault::Int(value) => write!(writer, "{value}"),
        ColumnDefault::UInt(value) => write!(writer, "{value}"),
        ColumnDefault::Float(value) => write!(writer, "{value}"),
        ColumnDefault::Text(value) => write_quoted_text(value, writer),
        ColumnDefault::Bool(true) => writer.write_all(b"TRUE"),
        ColumnDefault::Bool(false) => writer.write_all(b"FALSE"),
        ColumnDefault::CurrentTimestamp => writer.write_all(b"CURRENT_TIMESTAMP"),
        ColumnDefault::CurrentDate => writer.write_all(b"CURRENT_DATE"),
        ColumnDefault::CurrentTime => writer.write_all(b"CURRENT_TIME"),
        ColumnDefault::Raw(value) => writer.write_all(value.as_bytes()),
    }
}

/// Writes `value` wrapped in `delimiter` quotes, doubling any embedded delimiter.
///
/// Whole UTF-8 slices are written between delimiters rather than individual bytes,
/// so writers that validate each `write` chunk as UTF-8 (such as the string-backed
/// SQL writer) accept multibyte identifiers and literals like `café`.
fn write_quoted(value: &str, delimiter: char, writer: &mut impl Write) -> io::Result<()> {
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

fn write_quoted_text(value: &str, writer: &mut impl Write) -> io::Result<()> {
    write_quoted(value, '\'', writer)
}

/// Writes a single SQL identifier wrapped in double quotes, doubling any embedded
/// quotes. This keeps reserved words (`user`, `order`, ...) and identifiers with
/// special characters valid. Identifiers come from compile-time table metadata, so
/// this is robustness, not injection defense.
fn write_quoted_ident(value: &str, writer: &mut impl Write) -> io::Result<()> {
    write_quoted(value, '"', writer)
}

/// Writes a schema-qualified table reference with each part quoted separately,
/// e.g. `"public"."users"`.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn render_type(ty: SqlType) -> String {
        let mut out = Vec::new();
        write_pg_sql_type(&ty, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn fixed_bytes_check_names_are_stable_unique_and_bounded() {
        // Deterministic.
        assert_eq!(fixed_bytes_check_name("key"), fixed_bytes_check_name("key"));
        // Distinct columns get distinct names.
        assert_ne!(
            fixed_bytes_check_name("key"),
            fixed_bytes_check_name("nonce")
        );
        // Carries the recognizable prefix and never exceeds PostgreSQL's 63-byte identifier limit,
        // even for very long column names (so two columns can't truncate to the same name).
        let long = "a".repeat(200);
        let name = fixed_bytes_check_name(&long);
        assert!(name.starts_with(FIXED_BYTES_CHECK_PREFIX));
        assert!(name.len() <= 63, "name too long: {name}");
        assert_ne!(
            fixed_bytes_check_name(&long),
            fixed_bytes_check_name(&"a".repeat(199))
        );
    }

    #[test]
    fn postgres_types_map_to_postgres_ddl_types() {
        let cases = [
            (SqlType::Bool, "boolean"),
            (SqlType::I8, "smallint"),
            (SqlType::I16, "smallint"),
            (SqlType::I32, "integer"),
            (SqlType::I64, "bigint"),
            (SqlType::I128, "numeric"),
            (SqlType::Isize, "bigint"),
            (SqlType::U8, "smallint"),
            (SqlType::U16, "integer"),
            (SqlType::U32, "bigint"),
            (SqlType::U64, "numeric"),
            (SqlType::U128, "numeric"),
            (SqlType::Usize, "bigint"),
            (SqlType::F32, "real"),
            (SqlType::F64, "double precision"),
            (SqlType::String, "text"),
            (SqlType::Raw("jsonb".to_owned()), "jsonb"),
        ];

        for (ty, expected) in cases {
            assert_eq!(render_type(ty), expected);
        }
    }

    #[test]
    fn postgres_rejects_generated_column_without_expression() {
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
                }],
            }],
        };

        let mut out = Vec::new();
        let error = ddl::write_database(&model, &mut out).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("full_name"), "{error}");
    }

    #[test]
    fn postgres_renders_structured_types() {
        assert_eq!(render_type(SqlType::Varchar(64)), "varchar(64)");
        assert_eq!(render_type(SqlType::Char(2)), "char(2)");
        assert_eq!(render_type(SqlType::Text), "text");
        assert_eq!(
            render_type(SqlType::Decimal {
                precision: 10,
                scale: 2
            }),
            "numeric(10,2)"
        );
        assert_eq!(render_type(SqlType::Date), "date");
        assert_eq!(
            render_type(SqlType::Timestamp {
                tz: false,
                precision: None
            }),
            "timestamp"
        );
        assert_eq!(
            render_type(SqlType::Timestamp {
                tz: true,
                precision: None
            }),
            "timestamp with time zone"
        );
        // A fractional-seconds precision renders inside the base name, before the tz suffix.
        assert_eq!(
            render_type(SqlType::Timestamp {
                tz: true,
                precision: Some(6)
            }),
            "timestamp(6) with time zone"
        );
        assert_eq!(
            render_type(SqlType::Time {
                tz: false,
                precision: Some(3)
            }),
            "time(3)"
        );
        assert_eq!(render_type(SqlType::Uuid), "uuid");
        assert_eq!(render_type(SqlType::Jsonb), "jsonb");
        assert_eq!(render_type(SqlType::Bytes), "bytea");
    }
}
