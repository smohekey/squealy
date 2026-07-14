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

use std::collections::{HashMap, HashSet};

use squealy::{
    CheckModel, ColumnModel, DatabaseModel, DatabasePlan, DatabasePlanStep, DefaultValue,
    ForeignKeyModel, IndexDirection, IndexModel, SqlType, TableModel, TablePlanStep, ViewModel,
};

/// Renders ordered create-from-scratch DDL for a whole model. Statements are `;`-terminated and
/// newline-separated: tables (with inline PK/unique/check/foreign-keys), then indexes, then views in
/// dependency order. SQLite has no schemas, so schema names are dropped and all tables are flattened.
pub(crate) fn write_database(model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
    // SQLite keeps tables, indexes and views in one database-wide object namespace (there are no
    // schemas), so once schemas are flattened every table, index and view name must be unique —
    // including a table name that matches an index or a view. A model that relies on schema/table
    // scoping for those names is valid for the schema-aware backends but cannot be represented in
    // SQLite; reject it before rendering duplicate `CREATE …` statements. Tables are checked first, then
    // indexes, then views, so a collision is reported against the later-created object.
    let tables = || model.schemas.iter().flat_map(|schema| schema.tables.iter());
    let views = || model.schemas.iter().flat_map(|schema| schema.views.iter());
    let object_names = || {
        tables()
            .map(|table| table.name.as_str())
            .chain(tables().flat_map(|table| table.indexes.iter().map(|index| index.name.as_str())))
            .chain(views().map(|view| view.name.as_str()))
    };
    check_reserved_object_names(object_names())?;
    check_object_name_uniqueness(object_names())?;

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

    // Views are created last (all tables exist) and in dependency order, so a view that selects from
    // another view is created after it. The schema qualifier is suppressed by `SqliteDialect`.
    for (schema_name, view) in squealy::ordered_views(model) {
        statement(writer, &mut first)?;
        squealy::render_create_view(schema_name, view, false, &SqliteDialect, writer)?;
    }

    if !first {
        writer.write_all(b";")?;
    }
    Ok(())
}

/// Rejects any case-insensitive duplicate among SQLite object names. SQLite keeps tables and indexes
/// in one database-wide namespace and compares identifiers case-insensitively (ASCII-folded) even when
/// quoted, so every rendered name must be unique after schemas are flattened.
fn check_object_name_uniqueness<'a>(names: impl Iterator<Item = &'a str>) -> io::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(object_name_collision(name));
        }
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

/// Rejects any object name using a reserved prefix. SQLite forbids the `sqlite_` prefix for user
/// objects, and this backend keeps its schema-management bookkeeping in `__squealy_`-prefixed tables
/// that introspection filters out — so a user object with either prefix must be rejected before it is
/// created, or the next introspection would treat it as absent and churn a create/drop (or collide with
/// the metadata stores). The comparison is ASCII case-folded, matching how SQLite compares identifiers.
fn check_reserved_object_names<'a>(names: impl Iterator<Item = &'a str>) -> io::Result<()> {
    for name in names {
        let folded = name.to_ascii_lowercase();
        if folded.starts_with("sqlite_") || folded.starts_with("__squealy_") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "`{name}` uses a reserved object-name prefix: SQLite reserves `sqlite_`, and \
                     squealy reserves `__squealy_` for its schema-management bookkeeping tables"
                ),
            ));
        }
    }
    Ok(())
}

/// Renders a `CREATE TABLE` (plus any secondary indexes) for a query-builder [`squealy::Table`], used
/// by the `to::<T>()` create path and `Backend::write_table`. It lowers the table to a `TableModel` and
/// reuses the model renderer, so this single-table path is identical to `render_create` for that table
/// (no separate query-side rendering to drift out of sync).
pub(crate) fn write_table(
    table: &(dyn squealy::Table + Sync),
    writer: &mut impl Write,
) -> io::Result<()> {
    let model = squealy::table_from_dyn(table);
    // Same guards as `write_database`: reject reserved object-name prefixes, and require the table name
    // and its index names to be unique (an index whose name matches the table would collide in SQLite).
    let object_names = || {
        std::iter::once(model.name.as_str())
            .chain(model.indexes.iter().map(|index| index.name.as_str()))
    };
    check_reserved_object_names(object_names())?;
    check_object_name_uniqueness(object_names())?;
    write_create_table(&model, writer)?;
    for index in &model.indexes {
        writer.write_all(b";\n")?;
        write_create_index(&model.name, index, writer)?;
    }
    Ok(())
}

/// Renders an ordered incremental DDL plan. Statements are `;`-terminated and newline-separated,
/// matching [`write_database`].
///
/// SQLite's `ALTER TABLE` only adds/drops/renames columns and renames tables, so a change it cannot
/// express in place — a type change, or adding/dropping/altering a primary key, unique, foreign key or
/// check (all of which SQLite carries only inline in `CREATE TABLE`) — is applied by rebuilding the
/// whole table: create a new table with the target shape, copy the surviving data, drop the old table,
/// and rename the new one into its place. The rebuilt table's *unchanged* columns are not in the plan,
/// so its full shape comes from `desired`, the target model the plan was diffed to reach.
///
/// Referential integrity across the drop-and-recreate is the executor's responsibility: `DROP TABLE`
/// fires `ON DELETE` actions on child rows while foreign keys are enforced, so
/// [`DdlExecutor::execute_ddl`](squealy::DdlExecutor::execute_ddl) applies the whole batch with
/// enforcement disabled (which SQLite only allows outside a transaction) and re-validates with
/// `PRAGMA foreign_key_check` before committing. This renderer therefore emits no transaction control
/// or foreign-key pragmas, matching how the schema-aware backends leave the transaction to the executor.
pub(crate) fn write_plan(
    plan: &DatabasePlan,
    desired: &DatabaseModel,
    writer: &mut impl Write,
) -> io::Result<()> {
    // Validate the target object namespace up front, exactly as create-from-scratch does. SQLite keeps
    // tables, indexes and views in one database-wide namespace, so a target whose object names are not
    // unique cannot be represented; reject it here rather than emit a plan that fails partway (or leaves
    // the schema wrong). Tables are checked before indexes before views so a collision is reported
    // against the later-created object.
    let tables = || {
        desired
            .schemas
            .iter()
            .flat_map(|schema| schema.tables.iter())
    };
    let views = || {
        desired
            .schemas
            .iter()
            .flat_map(|schema| schema.views.iter())
    };
    let object_names = || {
        tables()
            .map(|table| table.name.as_str())
            .chain(tables().flat_map(|table| table.indexes.iter().map(|index| index.name.as_str())))
            .chain(views().map(|view| view.name.as_str()))
    };
    check_reserved_object_names(object_names())?;
    check_object_name_uniqueness(object_names())?;

    let mut first = true;

    // Create the refactor bookkeeping table once, up front, if any rename in the plan carries a
    // refactor id to record (mirrors the schema-aware backends' inline refactor log).
    if plan.steps.iter().any(plan_step_has_refactor_id) {
        statement(writer, &mut first)?;
        write_create_refactor_log_table(writer)?;
    }

    // Free every table and index name the plan drops or redefines, up front, before any create runs.
    // SQLite keeps tables and indexes in one database-wide namespace, so a dropped name may be reused
    // by a later create (an index moved between tables, or an index taking a dropped table's name);
    // doing all drops first releases those names and prevents a later per-table drop from destroying a
    // replacement another table already created. Genuine removals are handled here too, so `DropTable`
    // and `DropIndex` emit nothing in the main pass.
    for step in &plan.steps {
        match step {
            DatabasePlanStep::DropTable { table, .. } => {
                statement(writer, &mut first)?;
                writer.write_all(b"DROP TABLE IF EXISTS ")?;
                write_quoted_ident(&table.name, writer)?;
            }
            DatabasePlanStep::AlterTable { change, .. } => {
                let dropped_index = match change.as_ref() {
                    TablePlanStep::DropIndex { index } => Some(index.name.as_str()),
                    TablePlanStep::AlterIndex { before, .. } => Some(before.name.as_str()),
                    _ => None,
                };
                if let Some(name) = dropped_index {
                    statement(writer, &mut first)?;
                    writer.write_all(b"DROP INDEX IF EXISTS ")?;
                    write_quoted_ident(name, writer)?;
                }
            }
            _ => {}
        }
    }

    // Drop every view the plan touches, before any table is created, rebuilt or renamed. SQLite reparses
    // a view when a table it references is renamed — a table rebuild renames the new table into place —
    // and errors ("no such table") if that table is momentarily absent, so a live view over a rebuilt
    // table must be gone before the rebuild. Views the plan keeps are recreated by their `CreateView`
    // step in the main pass (in dependency order, after every table exists); removed views stay dropped.
    // `IF EXISTS` covers a brand-new view (nothing to drop) and a name a plan both drops and recreates.
    //
    // Skip a name a table still owns: a view that reuses a table's name (the table is dropped or renamed
    // away in this plan) is brand-new — it does not exist yet, so it needs no pre-drop — and
    // `DROP VIEW IF EXISTS "x"` errors with "use DROP TABLE" while `x` is still a table. A dropped table
    // was already freed by the pass above, but a rename happens later in the main pass, so exclude both.
    // The comparison is ASCII case-folded, matching how SQLite resolves identifiers (and the namespace
    // uniqueness check above): a view `x` reusing a table renamed from `X` must still be skipped.
    let table_owned_names: HashSet<String> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::DropTable { table, .. } => Some(table.name.to_ascii_lowercase()),
            DatabasePlanStep::RenameTable { from, .. } => Some(from.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    for step in &plan.steps {
        if let DatabasePlanStep::CreateView { view, .. } | DatabasePlanStep::DropView { view, .. } =
            step
            && !table_owned_names.contains(&view.name.to_ascii_lowercase())
        {
            statement(writer, &mut first)?;
            writer.write_all(b"DROP VIEW IF EXISTS ")?;
            write_quoted_ident(&view.name, writer)?;
        }
    }

    // A table rebuild (create-copy-drop-rename) renames the new table into place, and — inside the
    // executor's transaction — SQLite reparses every view when a referenced table is renamed, erroring if
    // that view's target (or, transitively, an ancestor view) is momentarily absent. The pre-pass above
    // drops every view the plan *changes*, but an *unchanged* view over a rebuilt table carries no
    // `Create`/`DropView` step, so it would survive the rebuild and hit that reparse error. (Before view
    // bodies were introspected, such a view was always re-created — and thus always pre-dropped — which
    // masked this; now that an unchanged body compares equal, it no longer is.) Collect the tables the
    // plan rebuilds, then the transitive closure of desired views over them: these "collateral" views are
    // dropped here and recreated after all table work (below).
    let rebuilt_tables: HashSet<String> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::AlterTable { table, change, .. } if change_needs_rebuild(change) => {
                Some(table.to_ascii_lowercase())
            }
            _ => None,
        })
        .collect();
    let plan_touched_views: HashSet<String> =
        plan.steps
            .iter()
            .filter_map(|step| match step {
                DatabasePlanStep::CreateView { view, .. }
                | DatabasePlanStep::DropView { view, .. } => Some(view.name.to_ascii_lowercase()),
                _ => None,
            })
            .collect();
    let all_desired_views: Vec<(Option<&str>, &ViewModel)> = desired
        .schemas
        .iter()
        .flat_map(|schema| {
            schema
                .views
                .iter()
                .map(move |view| (schema.name.as_deref(), view))
        })
        .collect();
    // The transitive closure of views affected by a rebuild: seed with the rebuilt table names, then
    // repeatedly add any desired view that references an already-affected name (a rebuilt table or an
    // already-affected view), to a fixpoint. A **plan-touched** view stays in the closure so its own
    // dependents still propagate (the plan already dropped it in the pre-pass), but is excluded from
    // `collateral_views` below — the plan recreates it itself.
    let mut affected = rebuilt_tables;
    if !affected.is_empty() {
        loop {
            let mut added = false;
            for (_, view) in &all_desired_views {
                let key = view.name.to_ascii_lowercase();
                if !affected.contains(&key)
                    && view
                        .referenced_sources()
                        .any(|source| affected.contains(&source.name.to_ascii_lowercase()))
                {
                    affected.insert(key);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
    }
    let collateral_views: Vec<(Option<&str>, &ViewModel)> = all_desired_views
        .iter()
        .filter(|(_, view)| {
            let key = view.name.to_ascii_lowercase();
            affected.contains(&key) && !plan_touched_views.contains(&key)
        })
        .copied()
        .collect();
    for (_, view) in &collateral_views {
        // The `table_owned_names` skip does not apply here — a collateral view is unchanged and already
        // exists — but keep it defensively, matching the pre-pass above.
        if !table_owned_names.contains(&view.name.to_ascii_lowercase()) {
            statement(writer, &mut first)?;
            writer.write_all(b"DROP VIEW IF EXISTS ")?;
            write_quoted_ident(&view.name, writer)?;
        }
    }

    // A rebuild subsumes *all* of a table's alterations, so a table's `AlterTable` steps are handled
    // together at the first one seen; later steps for the same table are skipped.
    let mut altered_tables = HashSet::new();

    for step in &plan.steps {
        match step {
            // SQLite has no schemas; creating or dropping the flattened (`None`) namespace is a no-op.
            // A named schema cannot be represented — canonicalization flattens names to `None`, so this
            // should not occur, but reject it rather than silently ignore a real name.
            DatabasePlanStep::CreateSchema { schema } | DatabasePlanStep::DropSchema { schema } => {
                if let Some(name) = schema {
                    return Err(named_schema_unsupported(name));
                }
            }
            DatabasePlanStep::CreateTable { table, .. } => {
                write_create_table_step(table, writer, &mut first)?;
            }
            // The table was already dropped by the drop pre-pass above; nothing to emit here.
            DatabasePlanStep::DropTable { .. } => {}
            DatabasePlanStep::RenameTable {
                refactor_id,
                from,
                to,
                ..
            } => {
                statement(writer, &mut first)?;
                writer.write_all(b"ALTER TABLE ")?;
                write_quoted_ident(from, writer)?;
                writer.write_all(b" RENAME TO ")?;
                write_quoted_ident(to, writer)?;
                if let Some(refactor_id) = refactor_id {
                    write_record_refactor(refactor_id, writer, &mut first)?;
                }
            }
            DatabasePlanStep::AlterTable { table, .. } => {
                if altered_tables.insert(table.clone()) {
                    write_table_alterations(table, plan, desired, writer, &mut first)?;
                }
            }
            DatabasePlanStep::CreateView { schema, view } => {
                // The view was already dropped in the up-front view pre-pass (SQLite has no
                // `CREATE OR REPLACE VIEW`); recreate it here, after all table work, so it binds to the
                // final table shapes. Views are ordered so a view-on-view is created after its source.
                statement(writer, &mut first)?;
                squealy::render_create_view(
                    schema.as_deref(),
                    view,
                    false,
                    &SqliteDialect,
                    writer,
                )?;
            }
            // The view was already dropped in the up-front view pre-pass; nothing to emit here.
            DatabasePlanStep::DropView { .. } => {}
        }
    }

    // Recreate the collateral views (unchanged views over a rebuilt table) dropped in the pre-pass, now
    // that every rebuilt table has been renamed back into place. SQLite does not validate a view's body
    // at `CREATE VIEW` time (only when the view is used, or a referenced table is renamed), so these need
    // no ordering among themselves or against the plan's own `CreateView` steps.
    for (schema, view) in &collateral_views {
        statement(writer, &mut first)?;
        squealy::render_create_view(*schema, view, false, &SqliteDialect, writer)?;
    }

    if !first {
        writer.write_all(b";")?;
    }
    Ok(())
}

/// Renders a plan's `CreateTable` step: the table (with inline constraints) plus its secondary indexes,
/// guarding the reserved object-name prefixes exactly as the create-from-scratch path does.
fn write_create_table_step(
    table: &TableModel,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    let object_names = || {
        std::iter::once(table.name.as_str())
            .chain(table.indexes.iter().map(|index| index.name.as_str()))
    };
    check_reserved_object_names(object_names())?;
    check_object_name_uniqueness(object_names())?;
    statement(writer, first)?;
    write_create_table(table, writer)?;
    for index in &table.indexes {
        write_plan_create_index(&table.name, index, writer, first)?;
    }
    Ok(())
}

/// Renders every alteration of one table. If any change is one SQLite's `ALTER TABLE` cannot express in
/// place, the whole table is rebuilt from `desired`; otherwise each change is emitted as a native
/// `ALTER TABLE`/`CREATE INDEX`/`DROP INDEX`. In both cases a renamed column's refactor id is recorded
/// afterwards (the rename itself happens natively or via the rebuild's copy mapping).
fn write_table_alterations(
    table: &str,
    plan: &DatabasePlan,
    desired: &DatabaseModel,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    let changes: Vec<&TablePlanStep> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            DatabasePlanStep::AlterTable {
                table: altered,
                change,
                ..
            } if altered == table => Some(change.as_ref()),
            _ => None,
        })
        .collect();

    if changes.iter().any(|change| change_needs_rebuild(change)) {
        write_table_rebuild(table, &changes, desired, writer, first)?;
    } else {
        for change in &changes {
            write_native_table_change(table, change, writer, first)?;
        }
    }

    for change in &changes {
        if let TablePlanStep::RenameColumn {
            refactor_id: Some(refactor_id),
            ..
        } = change
        {
            write_record_refactor(refactor_id, writer, first)?;
        }
    }
    Ok(())
}

/// Whether a table change is one SQLite's `ALTER TABLE` cannot express in place, forcing a table
/// rebuild. Primary keys, uniques, foreign keys and checks live only inline in `CREATE TABLE`, and
/// there is no `ALTER COLUMN`; column drops are also rebuilt because SQLite refuses to drop a column
/// used by a constraint or index and the plan step does not carry the table's other constraints.
fn change_needs_rebuild(change: &TablePlanStep) -> bool {
    match change {
        // SQLite has no table comment: a comment change is rejected (in the native path or, alongside a
        // rebuild, by `write_create_table`), not a rebuild trigger of its own.
        TablePlanStep::SetTableComment { .. } => false,
        // Natively expressible.
        TablePlanStep::RenameColumn { .. }
        | TablePlanStep::AddIndex { .. }
        | TablePlanStep::DropIndex { .. }
        | TablePlanStep::AlterIndex { .. } => false,
        TablePlanStep::AddColumn { column } => !native_addable(column),
        TablePlanStep::DropColumn { .. }
        | TablePlanStep::AlterColumn { .. }
        | TablePlanStep::AddPrimaryKey { .. }
        | TablePlanStep::DropPrimaryKey { .. }
        | TablePlanStep::AlterPrimaryKey { .. }
        | TablePlanStep::AddUnique { .. }
        | TablePlanStep::DropUnique { .. }
        | TablePlanStep::AlterUnique { .. }
        | TablePlanStep::AddForeignKey { .. }
        | TablePlanStep::DropForeignKey { .. }
        | TablePlanStep::AlterForeignKey { .. }
        | TablePlanStep::AddCheck { .. }
        | TablePlanStep::DropCheck { .. }
        | TablePlanStep::AlterCheck { .. } => true,
    }
}

/// Whether a column can be added with a native `ALTER TABLE … ADD COLUMN` (rather than a rebuild).
/// SQLite rejects adding a `PRIMARY KEY`/`UNIQUE` column (those arrive as separate constraint steps, so
/// a bare `AddColumn` is never one), an identity or generated column, a `NOT NULL` column without a
/// non-null constant default, or any column with a non-constant default (`CURRENT_*` / a raw
/// expression). A `COLLATE` clause is fine — `ALTER TABLE … ADD COLUMN … COLLATE …` is accepted.
fn native_addable(column: &ColumnModel) -> bool {
    if column.identity.is_some() || column.generated.is_some() {
        return false;
    }
    match &column.default {
        Some(
            DefaultValue::CurrentTimestamp
            | DefaultValue::CurrentDate
            | DefaultValue::CurrentTime
            | DefaultValue::Raw(_),
        ) => false,
        // A `NOT NULL` column needs a non-null constant default; an absent or `NULL` default is rejected.
        None | Some(DefaultValue::Null) => column.nullable,
        Some(_) => true,
    }
}

/// Renders a single natively-expressible table change. Only the variants
/// [`change_needs_rebuild`] classifies as non-rebuild reach here.
fn write_native_table_change(
    table: &str,
    change: &TablePlanStep,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    match change {
        // SQLite has no table comment (see `write_create_table`). Setting one is rejected — it cannot
        // round-trip and would churn every plan; clearing one (`after: None`) has nothing to emit.
        TablePlanStep::SetTableComment { after, .. } => {
            if after.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "SQLite table comments are not supported for schema management (table \
                         `{table}`): SQLite has no table comment and introspection cannot read one \
                         back, so a published comment would churn every plan"
                    ),
                ));
            }
        }
        TablePlanStep::AddColumn { column } => {
            statement(writer, first)?;
            writer.write_all(b"ALTER TABLE ")?;
            write_quoted_ident(table, writer)?;
            writer.write_all(b" ADD COLUMN ")?;
            write_column(column, writer)?;
        }
        // The refactor id (if any) is recorded by the caller.
        TablePlanStep::RenameColumn { from, to, .. } => {
            statement(writer, first)?;
            writer.write_all(b"ALTER TABLE ")?;
            write_quoted_ident(table, writer)?;
            writer.write_all(b" RENAME COLUMN ")?;
            write_quoted_ident(from, writer)?;
            writer.write_all(b" TO ")?;
            write_quoted_ident(to, writer)?;
        }
        TablePlanStep::AddIndex { index } => {
            write_plan_create_index(table, index, writer, first)?;
        }
        // The name was already freed by the drop pre-pass in `write_plan`; nothing to emit here.
        TablePlanStep::DropIndex { .. } => {}
        // The old definition was dropped by the pre-pass (a diff keys indexes by name, so
        // `before.name == after.name`); recreate the new definition.
        TablePlanStep::AlterIndex { after, .. } => {
            write_plan_create_index(table, after, writer, first)?;
        }
        other => {
            // Unreachable: `write_table_alterations` routes every rebuild-requiring change through
            // `write_table_rebuild`. Guard defensively rather than panic in a renderer.
            return Err(io::Error::other(format!(
                "internal error: {other:?} is not a native SQLite table change"
            )));
        }
    }
    Ok(())
}

/// How a rebuilt table's column is populated from the old table: a source column (copied verbatim,
/// possibly under a different name after a rename) or a `cast-column` conversion expression.
enum CopySource<'a> {
    Column(&'a str),
    Expression(&'a str),
}

/// A rowid alias (`rowid` / `_rowid_` / `oid`) not shadowed by a user column of `table`. SQLite
/// resolves a bare rowid name to a real column of that name if one exists, so a full-column-replace
/// rebuild — which binds the hidden rowid to carry the row count — must pick an alias the table does
/// not define. Errors only if the table defines columns shadowing all three (which would also make the
/// rowid unaddressable in ordinary SQL).
fn unshadowed_rowid_alias(table: &TableModel) -> io::Result<&'static str> {
    let shadowed: HashSet<String> = table
        .columns
        .iter()
        .map(|column| column.name.to_ascii_lowercase())
        .collect();
    ["rowid", "_rowid_", "oid"]
        .into_iter()
        .find(|alias| !shadowed.contains(*alias))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "cannot rebuild `{}` while preserving its rows: it defines columns shadowing every \
                     rowid alias (rowid, _rowid_, oid)",
                    table.name
                ),
            )
        })
}

/// Rebuilds a table whose change SQLite's `ALTER TABLE` cannot express in place: create the new shape
/// under a temporary name, copy the surviving data, drop the old table, rename the new one into place,
/// and recreate the target's indexes (dropping the old table dropped them). Data is carried column by
/// column: an added column takes its default (omitted from the copy) and a renamed column is copied
/// from its old name.
fn write_table_rebuild(
    table: &str,
    changes: &[&TablePlanStep],
    desired: &DatabaseModel,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    // The full target table. SQLite has no schemas, so the (flattened) table name is unique across the
    // model; find it wherever it sits.
    let target = desired
        .schemas
        .iter()
        .flat_map(|schema| &schema.tables)
        .find(|candidate| candidate.name == table)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot render a SQLite rebuild of `{table}`: the target table is not in the \
                     desired model passed to render_plan"
                ),
            )
        })?;

    // Whether the target keeps an `AUTOINCREMENT` high-water mark to carry over (see below).
    // `write_create_table` re-validates the same identity rules, so an invalid identity still surfaces.
    let has_autoincrement = autoincrement_column(target)?.is_some();

    // A temporary name that never persists (it is renamed to the real name within the same batch). The
    // `__squealy_` prefix is reserved for this backend's bookkeeping, so it cannot collide with a user
    // table (and two rebuilt tables get distinct temp names).
    let temp_name = format!("__squealy_new_{table}");
    let mut temp_table = target.clone();
    temp_table.name = temp_name.clone();
    statement(writer, first)?;
    write_create_table(&temp_table, writer)?;

    // Copy the surviving data. A target column is copied from the old table unless it is newly added
    // (it takes its default) or renamed (copied from its old name).
    let added: HashSet<&str> = changes
        .iter()
        .filter_map(|change| match change {
            TablePlanStep::AddColumn { column } => Some(column.name.as_str()),
            _ => None,
        })
        .collect();
    let renamed_from: HashMap<&str, &str> = changes
        .iter()
        .filter_map(|change| match change {
            TablePlanStep::RenameColumn { from, to, .. } => Some((to.as_str(), from.as_str())),
            _ => None,
        })
        .collect();
    // A `cast-column` refactor supplies a conversion expression on the column's type change, carried on
    // the `AlterColumn` step. It replaces the plain copy so the migration actually evaluates the
    // conversion (the expression is backend-specific SQL, emitted verbatim, and references the old
    // table's columns).
    let type_casts: HashMap<&str, &str> = changes
        .iter()
        .filter_map(|change| match change {
            TablePlanStep::AlterColumn {
                after,
                type_cast: Some(expression),
                ..
            } => Some((after.name.as_str(), expression.as_str())),
            _ => None,
        })
        .collect();
    let carried: Vec<(&str, CopySource)> = target
        .columns
        .iter()
        .filter(|column| !added.contains(column.name.as_str()))
        .map(|column| {
            let source = if let Some(expression) = type_casts.get(column.name.as_str()) {
                CopySource::Expression(expression)
            } else {
                CopySource::Column(
                    renamed_from
                        .get(column.name.as_str())
                        .copied()
                        .unwrap_or(column.name.as_str()),
                )
            };
            (column.name.as_str(), source)
        })
        .collect();

    if carried.is_empty() {
        // Every target column is newly added, but the rows must still survive: selecting no columns
        // would copy zero rows and silently drop the whole table. Insert one row per old row, binding
        // only the hidden rowid (auto-assigned via `NULL`) — no user column on either side, so every
        // real column takes its default. A bare rowid name resolves to a user column that shadows it,
        // so bind an alias the target does not define, and select from the old table without naming any
        // of its columns (its rowid may be shadowed too).
        let rowid_alias = unshadowed_rowid_alias(target)?;
        statement(writer, first)?;
        writer.write_all(b"INSERT INTO ")?;
        write_quoted_ident(&temp_name, writer)?;
        writer.write_all(b" (")?;
        write_quoted_ident(rowid_alias, writer)?;
        writer.write_all(b")\nSELECT NULL FROM ")?;
        write_quoted_ident(table, writer)?;
    } else {
        statement(writer, first)?;
        writer.write_all(b"INSERT INTO ")?;
        write_quoted_ident(&temp_name, writer)?;
        writer.write_all(b" (")?;
        for (position, (target_column, _)) in carried.iter().enumerate() {
            if position > 0 {
                writer.write_all(b", ")?;
            }
            write_quoted_ident(target_column, writer)?;
        }
        writer.write_all(b")\nSELECT ")?;
        for (position, (_, source)) in carried.iter().enumerate() {
            if position > 0 {
                writer.write_all(b", ")?;
            }
            match source {
                CopySource::Column(name) => write_quoted_ident(name, writer)?,
                CopySource::Expression(expression) => writer.write_all(expression.as_bytes())?,
            }
        }
        writer.write_all(b" FROM ")?;
        write_quoted_ident(table, writer)?;
    }

    // Carry the AUTOINCREMENT high-water mark. Dropping the old table removes its `sqlite_sequence`
    // row, and copying rows only advances the new table's sequence to the highest *surviving* id — so a
    // row deleted before the rebuild could have its id handed out again, which AUTOINCREMENT promises
    // never to do. While both tables' `sqlite_sequence` rows still exist (the new table's is created
    // lazily, so ensure it), raise the new table's mark to the old table's; the later rename carries it
    // onto the real name.
    if has_autoincrement {
        statement(writer, first)?;
        writer.write_all(b"INSERT INTO \"sqlite_sequence\" (\"name\", \"seq\") SELECT ")?;
        write_quoted_text(&temp_name, writer)?;
        writer.write_all(
            b", 0 WHERE NOT EXISTS (SELECT 1 FROM \"sqlite_sequence\" WHERE \"name\" = ",
        )?;
        write_quoted_text(&temp_name, writer)?;
        writer.write_all(b")")?;

        statement(writer, first)?;
        writer.write_all(b"UPDATE \"sqlite_sequence\" SET \"seq\" = max(\"seq\", coalesce((SELECT \"seq\" FROM \"sqlite_sequence\" WHERE \"name\" = ")?;
        write_quoted_text(table, writer)?;
        writer.write_all(b"), 0)) WHERE \"name\" = ")?;
        write_quoted_text(&temp_name, writer)?;
    }

    statement(writer, first)?;
    writer.write_all(b"DROP TABLE ")?;
    write_quoted_ident(table, writer)?;

    statement(writer, first)?;
    writer.write_all(b"ALTER TABLE ")?;
    write_quoted_ident(&temp_name, writer)?;
    writer.write_all(b" RENAME TO ")?;
    write_quoted_ident(table, writer)?;

    for index in &target.indexes {
        write_plan_create_index(table, index, writer, first)?;
    }
    Ok(())
}

/// Whether a plan step carries a refactor id that must be recorded in the bookkeeping table.
fn plan_step_has_refactor_id(step: &DatabasePlanStep) -> bool {
    match step {
        DatabasePlanStep::RenameTable { refactor_id, .. } => refactor_id.is_some(),
        DatabasePlanStep::AlterTable { change, .. } => matches!(
            change.as_ref(),
            TablePlanStep::RenameColumn {
                refactor_id: Some(_),
                ..
            }
        ),
        _ => false,
    }
}

/// Creates the refactor bookkeeping table if absent. Its shape matches
/// [`SchemaRefactorStore`](squealy::SchemaRefactorStore)'s so a plan-recorded id reads back through the
/// store.
fn write_create_refactor_log_table(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(
        b"CREATE TABLE IF NOT EXISTS \"__squealy_refactors\" (\
\"id\" TEXT NOT NULL PRIMARY KEY, \
\"applied_at\" TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
}

/// Records an applied refactor id (idempotent: `INSERT OR IGNORE`).
fn write_record_refactor(
    refactor_id: &str,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    statement(writer, first)?;
    writer.write_all(b"INSERT OR IGNORE INTO \"__squealy_refactors\" (\"id\") VALUES (")?;
    write_quoted_text(refactor_id, writer)?;
    writer.write_all(b")")
}

fn named_schema_unsupported(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("SQLite has no schemas, so the namespace `{name}` cannot be created or dropped"),
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
    // SQLite has no table comment and introspection cannot read one back (there is no PRAGMA for it),
    // so a published comment would diff as a never-settling `SetTableComment` on every plan. Reject it
    // rather than silently drop it, matching how table CHECK constraints and column collations are
    // handled — a rebuild goes through here too, so a commented target is rejected there as well.
    if table.comment.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "SQLite table comments are not supported for schema management (table `{}`): SQLite \
                 has no table comment and introspection cannot read one back, so a published comment \
                 would churn every plan",
                table.name
            ),
        ));
    }

    // SQLite has no column prefix lengths (`col(n)`), and a constraint change rebuilds through here, so
    // reject a `UNIQUE`/`PRIMARY KEY` carrying one rather than silently drop it and emit a full-column
    // constraint that would never round-trip (mirrors the index-metadata reject in `write_create_index`).
    for constraint in table.primary_key.iter().chain(&table.uniques) {
        if !constraint.prefix_lengths.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "SQLite does not support constraint column prefix lengths (constraint `{}`)",
                    constraint.name
                ),
            ));
        }
    }

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
/// Rejects an `ON UPDATE CURRENT_TIMESTAMP` attribute, which is MySQL-only and SQLite cannot represent.
/// The incremental plan render path does not validate capabilities, so both column-rendering paths
/// (the general one and the `INTEGER PRIMARY KEY AUTOINCREMENT` special case) reject it rather than
/// silently dropping it — which would churn a table rebuild on every plan.
fn reject_on_update(column: &ColumnModel) -> io::Result<()> {
    if column.on_update.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "SQLite does not support an `ON UPDATE` column attribute (column `{}`)",
                column.name
            ),
        ));
    }
    Ok(())
}

fn write_autoincrement_column(column: &ColumnModel, writer: &mut impl Write) -> io::Result<()> {
    reject_on_update(column)?;
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
    reject_on_update(column)?;
    if column.comment.is_some() {
        // Like a table comment, a column comment is not introspectable, so it would churn every plan.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "SQLite column comments are not supported for schema management (column `{}`): SQLite \
                 has no column comment and introspection cannot read one back, so a published comment \
                 would churn every plan",
                column.name
            ),
        ));
    }
    write_quoted_ident(&column.name, writer)?;
    writer.write_all(b" ")?;
    writer.write_all(sqlite_affinity(&column.ty).as_bytes())?;
    // A `COLLATE` clause is a column constraint that carries the collating sequence for text comparison.
    // SQLite exposes it only in the `CREATE TABLE` text (no PRAGMA reports it), so introspection recovers
    // it by parsing that text. The name is quoted like any identifier so a registered collation whose
    // name needs quoting (spaces, `-`, an embedded quote) still parses; SQLite accepts a quoted collation
    // name, and the introspector unquotes it back (SQLite's built-ins `BINARY`/`NOCASE`/`RTRIM` quote
    // harmlessly).
    if let Some(collation) = &column.collation {
        writer.write_all(b" COLLATE ")?;
        write_quoted_ident(collation, writer)?;
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
    // Rendered inline and unnamed: SQLite exposes a `CHECK` only in the `CREATE TABLE` text (no PRAGMA),
    // so introspection recovers the expression but not a name — the desired and introspected sides are
    // matched by a name derived from the expression (see `Sqlite::canonical_check_name`), making the
    // rendered name redundant. The expression is written verbatim between parentheses, trimmed so it
    // round-trips against the trimmed form introspection reads back (see `canonical_check_expression`).
    // (The fixed-width `[u8; N]` column check is rendered inline in `write_column`, not here.)
    //
    // SQLite has no `NOT VALID`/`NOT ENFORCED` for a check: rendering one silently would turn a package
    // model's validation/enforcement metadata into a plain, immediately-enforced constraint (or fail the
    // migration on existing rows). Reject it instead — as `write_foreign_key` rejects the same metadata —
    // rather than drop it. (A crate `#[check]` always leaves both `None`; this guards hand-written or
    // packaged models. The render path does not run `check_create`, so the guard belongs here.)
    if check.validation.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "SQLite does not support CHECK constraint validation metadata (constraint `{}`)",
                check.name
            ),
        ));
    }
    if check.enforcement.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "SQLite does not support CHECK constraint enforcement metadata (constraint `{}`)",
                check.name
            ),
        ));
    }
    writer.write_all(b"CHECK (")?;
    squealy::render_scalar_expr(&check.expression, &SqliteDialect, writer)?;
    writer.write_all(b")")
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

/// Renders a plan-applied index creation: `DROP INDEX IF EXISTS "name"` then `CREATE INDEX …`.
///
/// SQLite's index names are database-wide, so a name may still be held by an obsolete index on another
/// table when this runs — the plan's per-table drops and adds interleave, and a name can even be moved
/// or swapped between tables (`a.idx`/`b.idy` → `a.idy`/`b.idx`). Freeing the name first lets the create
/// succeed instead of failing on a stale duplicate. The up-front target object-name uniqueness check
/// guarantees the name's current holder is obsolete (the target uses each name once), so dropping it is
/// always correct. Create-from-scratch uses [`write_create_index`] directly (nothing pre-exists).
fn write_plan_create_index(
    table: &str,
    index: &IndexModel,
    writer: &mut impl Write,
    first: &mut bool,
) -> io::Result<()> {
    statement(writer, first)?;
    writer.write_all(b"DROP INDEX IF EXISTS ")?;
    write_quoted_ident(&index.name, writer)?;
    statement(writer, first)?;
    write_create_index(table, index, writer)
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
    if !index.prefix_lengths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite does not support index column prefix lengths",
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
        writer.write_all(b" WHERE ")?;
        squealy::render_scalar_expr(predicate, &SqliteDialect, writer)?;
    }
    Ok(())
}

/// The SQLite type affinity for a neutral [`SqlType`]. SQLite is dynamically typed, so the column type
/// only assigns one of five affinities; this is reused by [`SqliteDialect::write_cast_type`] and by
/// introspection's `canonical_sql_type` (to collapse a desired type to the same affinity read back).
pub(crate) fn sqlite_affinity(ty: &SqlType) -> &str {
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
/// `CAST` affinity names, plus the SQLite spellings for the seams the schema-aware backends default
/// differently — schema suppression (`qualify_schema`), `length`/`substr`/`||` builtins, `RETURNING`
/// without a target alias, and the `SELECT * FROM (…)` set-operand wrapper. Everything else uses the
/// trait defaults, which already match SQLite (integer-division float cast, `DEFAULT VALUES` empty
/// inserts, `NULLS FIRST`/`LAST`, `ON CONFLICT` upserts, `UPDATE … FROM`). Both the query renderer and
/// the shared view-body renderer render through this.
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

    fn unary_string_fn_name(&self, func: squealy::UnaryStringFunc) -> &'static str {
        match func {
            // SQLite has no `CHAR_LENGTH`; `length()` counts characters for TEXT values.
            squealy::UnaryStringFunc::Length => "length",
            other => other.sql_name(),
        }
    }

    fn qualify_schema(&self) -> bool {
        // SQLite has no schemas; table names render unqualified (matching the flattened DDL).
        false
    }

    fn returning_omits_target_alias(&self) -> bool {
        // SQLite's UPDATE/DELETE `RETURNING` cannot resolve the target-table alias (`no such column:
        // q0_0.col`); a single-table statement is unambiguous, so the columns render bare.
        true
    }

    fn set_operand_style(&self) -> squealy::SetOperandStyle {
        // SQLite rejects a parenthesized compound operand and a per-operand `ORDER BY`/`LIMIT`, so an
        // operand is wrapped as `SELECT * FROM (SELECT …)` (valid for ordered/limited/nested operands).
        squealy::SetOperandStyle::SubquerySelect
    }

    fn supports_intersect_except_all(&self) -> bool {
        // SQLite allows `ALL` only after `UNION`; `INTERSECT ALL`/`EXCEPT ALL` are syntax errors.
        false
    }

    fn substring_uses_function_call(&self) -> bool {
        // SQLite spells substring as `substr(s, start, len)`, not `SUBSTRING(s FROM start FOR len)`.
        true
    }

    fn supports_parenthesized_recursive_cte_arm(&self) -> bool {
        // SQLite's recursive-CTE grammar rejects any parenthesized recursive arm, so an arm carrying its
        // own ORDER BY/LIMIT/OFFSET (which needs parens to scope) has no valid rendering and is rejected.
        false
    }

    fn concat_uses_pipe_operator(&self) -> bool {
        // SQLite has no null-propagating `CONCAT`; `||` returns NULL if either operand is NULL,
        // matching squealy's concat expression (nullable iff either operand is nullable).
        true
    }

    fn delete_using_style(&self) -> squealy::DeleteUsingStyle {
        // SQLite has no join-delete; a correlated delete becomes `DELETE … WHERE EXISTS (SELECT …)`.
        squealy::DeleteUsingStyle::SqliteExists
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
        assert_eq!(
            sqlite_affinity(&SqlType::Timestamp {
                tz: true,
                precision: Some(6)
            }),
            "TEXT"
        );
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
