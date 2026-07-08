//! Name-based diffing for owned schema models.
//!
//! This module compares a desired [`DatabaseModel`] with an actual [`DatabaseModel`] and reports the
//! structural changes needed to make the actual model match the desired model. It does not infer
//! renames or render SQL; those are later planning/rendering steps.

use std::collections::{BTreeMap, BTreeSet};

use squealy::{
    CheckModel, ColumnModel, Constraint, DatabaseModel, ForeignKeyModel, IndexModel, SchemaModel,
    TableModel, ViewColumnModel, ViewModel,
};

/// The structured diff from an actual database model to a desired database model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DatabaseDiff {
    pub changes: Vec<DatabaseDiffChange>,
}

impl DatabaseDiff {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn classified_changes(&self) -> Vec<ClassifiedDatabaseDiffChange> {
        self.changes
            .iter()
            .map(|change| ClassifiedDatabaseDiffChange {
                risk: change.risk(),
                change: change.clone(),
            })
            .collect()
    }
}

/// A diff change with conservative deployment-risk classification.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassifiedDatabaseDiffChange {
    pub risk: ChangeRisk,
    pub change: DatabaseDiffChange,
}

/// Conservative risk classification for a schema change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeRisk {
    Safe,
    Destructive,
    Ambiguous,
}

/// Policy for deciding whether a diff is acceptable to apply automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffPolicy {
    pub allow_destructive: bool,
    pub allow_ambiguous: bool,
}

impl DiffPolicy {
    pub const BLOCK_RISKY: Self = Self {
        allow_destructive: false,
        allow_ambiguous: false,
    };

    pub const ALLOW_ALL: Self = Self {
        allow_destructive: true,
        allow_ambiguous: true,
    };

    pub fn allows(self, risk: ChangeRisk) -> bool {
        match risk {
            ChangeRisk::Safe => true,
            ChangeRisk::Destructive => self.allow_destructive,
            ChangeRisk::Ambiguous => self.allow_ambiguous,
        }
    }
}

impl Default for DiffPolicy {
    fn default() -> Self {
        Self::BLOCK_RISKY
    }
}

/// Error returned when a diff contains changes blocked by a [`DiffPolicy`].
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("diff contains {} blocked change(s)", blocked.len())]
pub struct DiffPolicyError {
    pub blocked: Vec<ClassifiedDatabaseDiffChange>,
}

/// Checks whether `diff` is allowed by `policy`.
pub fn check_diff_policy(diff: &DatabaseDiff, policy: DiffPolicy) -> Result<(), DiffPolicyError> {
    let blocked = diff
        .classified_changes()
        .into_iter()
        .filter(|change| !policy.allows(change.risk))
        .collect::<Vec<_>>();

    if blocked.is_empty() {
        Ok(())
    } else {
        Err(DiffPolicyError { blocked })
    }
}

/// A database-level change.
#[derive(Clone, Debug, PartialEq)]
pub enum DatabaseDiffChange {
    CreateSchema {
        schema: Option<String>,
    },
    DropSchema {
        schema: Option<String>,
    },
    CreateTable {
        schema: Option<String>,
        table: TableModel,
    },
    DropTable {
        schema: Option<String>,
        table: TableModel,
    },
    AlterTable {
        schema: Option<String>,
        table: String,
        changes: Vec<TableDiffChange>,
    },
    /// Create-or-replace a view. A view-body change is expressed as a single `CreateView` (rendered
    /// `CREATE OR REPLACE VIEW`); a change that alters the column set is a `DropView` + `CreateView`.
    CreateView {
        schema: Option<String>,
        view: ViewModel,
    },
    DropView {
        schema: Option<String>,
        view: ViewModel,
    },
}

impl DatabaseDiffChange {
    pub fn risk(&self) -> ChangeRisk {
        match self {
            DatabaseDiffChange::CreateSchema { .. }
            | DatabaseDiffChange::CreateTable { .. }
            // Create-or-replace of a view loses no data and can be re-run.
            | DatabaseDiffChange::CreateView { .. } => ChangeRisk::Safe,
            DatabaseDiffChange::DropSchema { .. }
            | DatabaseDiffChange::DropTable { .. }
            | DatabaseDiffChange::DropView { .. } => ChangeRisk::Destructive,
            DatabaseDiffChange::AlterTable { changes, .. } => classify_table_changes(changes),
        }
    }
}

/// A table-level change.
#[derive(Clone, Debug, PartialEq)]
pub enum TableDiffChange {
    SetTableComment {
        before: Option<String>,
        after: Option<String>,
    },
    AddColumn {
        column: ColumnModel,
    },
    DropColumn {
        column: ColumnModel,
    },
    AlterColumn {
        before: ColumnModel,
        after: ColumnModel,
    },
    AddPrimaryKey {
        constraint: Constraint,
    },
    DropPrimaryKey {
        constraint: Constraint,
    },
    AlterPrimaryKey {
        before: Constraint,
        after: Constraint,
    },
    AddUnique {
        constraint: Constraint,
    },
    DropUnique {
        constraint: Constraint,
    },
    AlterUnique {
        before: Constraint,
        after: Constraint,
    },
    AddForeignKey {
        foreign_key: ForeignKeyModel,
    },
    DropForeignKey {
        foreign_key: ForeignKeyModel,
    },
    AlterForeignKey {
        before: ForeignKeyModel,
        after: ForeignKeyModel,
    },
    AddCheck {
        check: CheckModel,
    },
    DropCheck {
        check: CheckModel,
    },
    AlterCheck {
        before: CheckModel,
        after: CheckModel,
    },
    AddIndex {
        index: IndexModel,
    },
    DropIndex {
        index: IndexModel,
    },
    AlterIndex {
        before: IndexModel,
        after: IndexModel,
    },
}

impl TableDiffChange {
    pub fn risk(&self) -> ChangeRisk {
        match self {
            TableDiffChange::SetTableComment { .. }
            | TableDiffChange::AddPrimaryKey { .. }
            | TableDiffChange::AddUnique { .. }
            | TableDiffChange::AddForeignKey { .. }
            | TableDiffChange::AddCheck { .. }
            | TableDiffChange::AddIndex { .. } => ChangeRisk::Safe,
            TableDiffChange::DropColumn { .. }
            | TableDiffChange::DropPrimaryKey { .. }
            | TableDiffChange::DropUnique { .. }
            | TableDiffChange::DropForeignKey { .. }
            | TableDiffChange::DropCheck { .. }
            | TableDiffChange::DropIndex { .. } => ChangeRisk::Destructive,
            TableDiffChange::AddColumn { column } => {
                if column.nullable || column.default.is_some() || column.identity.is_some() {
                    ChangeRisk::Safe
                } else {
                    ChangeRisk::Ambiguous
                }
            }
            TableDiffChange::AlterColumn { .. }
            | TableDiffChange::AlterPrimaryKey { .. }
            | TableDiffChange::AlterUnique { .. }
            | TableDiffChange::AlterForeignKey { .. }
            | TableDiffChange::AlterCheck { .. }
            | TableDiffChange::AlterIndex { .. } => ChangeRisk::Ambiguous,
        }
    }
}

fn classify_table_changes(changes: &[TableDiffChange]) -> ChangeRisk {
    let mut risk = ChangeRisk::Safe;
    for change in changes {
        match change.risk() {
            ChangeRisk::Destructive => return ChangeRisk::Destructive,
            ChangeRisk::Ambiguous => risk = ChangeRisk::Ambiguous,
            ChangeRisk::Safe => {}
        }
    }
    risk
}

/// Compares `desired` with `actual`.
///
/// Returned changes are phrased as actions required to make `actual` match `desired`. Identity is
/// name-based: schema name, table name within schema, column name within table, and constraint/index
/// name within table.
pub fn diff_models(desired: &DatabaseModel, actual: &DatabaseModel) -> DatabaseDiff {
    let desired_schemas = keyed_schemas(&desired.schemas);
    let actual_schemas = keyed_schemas(&actual.schemas);
    let mut changes = Vec::new();

    // Views are diffed across the whole model (not one schema at a time) so a view can depend on a view
    // in another schema. Drops run before every table/schema change (a view may select from a table
    // being dropped) and creates run after all of them (a view may select from a table or schema being
    // added); the two phases bracket the per-schema table work below.
    let (view_drops, view_creates) = diff_views_global(desired, actual);
    changes.extend(view_drops);

    for schema_key in sorted_keys(&desired_schemas, &actual_schemas) {
        match (
            desired_schemas.get(&schema_key),
            actual_schemas.get(&schema_key),
        ) {
            (Some(desired_schema), None) => {
                changes.push(DatabaseDiffChange::CreateSchema {
                    schema: desired_schema.name.clone(),
                });
                for table in &desired_schema.tables {
                    changes.push(DatabaseDiffChange::CreateTable {
                        schema: desired_schema.name.clone(),
                        table: table.clone(),
                    });
                }
                // This schema's views are created in the global create phase, after every table exists.
            }
            (None, Some(actual_schema)) => {
                // This schema's views are dropped in the global drop phase, before any table.
                for table in &actual_schema.tables {
                    changes.push(DatabaseDiffChange::DropTable {
                        schema: actual_schema.name.clone(),
                        table: table.clone(),
                    });
                }
                changes.push(DatabaseDiffChange::DropSchema {
                    schema: actual_schema.name.clone(),
                });
            }
            (Some(desired_schema), Some(actual_schema)) => {
                diff_schema_tables(desired_schema, actual_schema, &mut changes);
            }
            (None, None) => {}
        }
    }

    changes.extend(view_creates);
    DatabaseDiff { changes }
}

fn diff_schema_tables(
    desired: &SchemaModel,
    actual: &SchemaModel,
    changes: &mut Vec<DatabaseDiffChange>,
) {
    let desired_tables = keyed_tables(&desired.tables);
    let actual_tables = keyed_tables(&actual.tables);

    for table_name in sorted_keys(&desired_tables, &actual_tables) {
        match (
            desired_tables.get(&table_name),
            actual_tables.get(&table_name),
        ) {
            (Some(desired_table), None) => changes.push(DatabaseDiffChange::CreateTable {
                schema: desired.name.clone(),
                table: (*desired_table).clone(),
            }),
            (None, Some(actual_table)) => changes.push(DatabaseDiffChange::DropTable {
                schema: actual.name.clone(),
                table: (*actual_table).clone(),
            }),
            (Some(desired_table), Some(actual_table)) => {
                let table_changes = diff_table(desired_table, actual_table);
                if !table_changes.is_empty() {
                    changes.push(DatabaseDiffChange::AlterTable {
                        schema: desired.name.clone(),
                        table: table_name,
                        changes: table_changes,
                    });
                }
            }
            (None, None) => {}
        }
    }
}

/// Diffs every view in the model, returning `(drops, creates)` so the caller can emit drops before
/// table changes and creates after. A view present in both that differs becomes a create-or-replace
/// (`CreateView`); when its column set changes it also needs a preceding `DropView`, since
/// `CREATE OR REPLACE VIEW` cannot rename or retype columns.
/// A view's global identity across the model: its schema (`None` = the unqualified schema) and name.
type ViewKey = (Option<String>, String);

/// Every view in the model, keyed by `(schema, name)`.
fn all_views(model: &DatabaseModel) -> BTreeMap<ViewKey, &ViewModel> {
    let mut views = BTreeMap::new();
    for schema in &model.schemas {
        for view in &schema.views {
            views.insert((schema.name.clone(), view.name.clone()), view);
        }
    }
    views
}

/// The `(schema, name)` a `CreateView`/`DropView` change targets.
fn view_change_key(change: &DatabaseDiffChange) -> ViewKey {
    match change {
        DatabaseDiffChange::CreateView { schema, view }
        | DatabaseDiffChange::DropView { schema, view } => (schema.clone(), view.name.clone()),
        _ => (None, String::new()),
    }
}

fn diff_views_global(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
) -> (Vec<DatabaseDiffChange>, Vec<DatabaseDiffChange>) {
    let desired_views = all_views(desired);
    let actual_views = all_views(actual);

    let mut drops = Vec::new();
    let mut creates = Vec::new();
    for key in sorted_keys(&desired_views, &actual_views) {
        let schema = key.0.clone();
        match (desired_views.get(&key), actual_views.get(&key)) {
            (Some(desired_view), None) => creates.push(DatabaseDiffChange::CreateView {
                schema,
                view: (*desired_view).clone(),
            }),
            (None, Some(actual_view)) => drops.push(DatabaseDiffChange::DropView {
                schema,
                view: (*actual_view).clone(),
            }),
            (Some(desired_view), Some(actual_view)) => {
                // A live-introspected view carries no structural body (it can't be reconstructed from
                // the stored SQL), so its body cannot be compared against the desired one. Rather than
                // treat a same-shape view as unchanged (which would never re-apply a changed `SELECT`
                // body), conservatively re-apply the desired definition as a `CREATE OR REPLACE VIEW`
                // every run — idempotent and non-destructive. The replace can't change the column set,
                // so a column change still drops first. Per-column nullability is unreliable when
                // introspected (PostgreSQL's `pg_attribute.attnotnull` is usually false for view
                // outputs) and a view's DDL carries no per-column NOT NULL, so the column comparison
                // ignores it. Models that carry a body (e.g. from a package) are compared in full.
                //
                // An introspected view whose body could not be reconstructed has an empty `SELECT` body
                // (only its name, columns, and dependencies are recovered) — the "body unknown" marker.
                if actual_view.query.is_empty() {
                    if view_columns_differ_ignoring_nullability(
                        &desired_view.columns,
                        &actual_view.columns,
                    ) {
                        drops.push(DatabaseDiffChange::DropView {
                            schema: schema.clone(),
                            view: (*actual_view).clone(),
                        });
                    }
                    creates.push(DatabaseDiffChange::CreateView {
                        schema,
                        view: (*desired_view).clone(),
                    });
                } else if desired_view != actual_view {
                    if desired_view.columns != actual_view.columns {
                        drops.push(DatabaseDiffChange::DropView {
                            schema: schema.clone(),
                            view: (*actual_view).clone(),
                        });
                    }
                    creates.push(DatabaseDiffChange::CreateView {
                        schema,
                        view: (*desired_view).clone(),
                    });
                }
            }
            (None, None) => {}
        }
    }

    // A view cannot be dropped while another live view still depends on it, so dropping/recreating one
    // (for a column-set change or a removal) forces its transitive dependents — in any schema — to be
    // dropped first and recreated after. Expand the drop/recreate set to that closure over the live
    // dependency graph; the sort below puts each dependent ahead of the view it selects from.
    let mut dropped: BTreeSet<ViewKey> = drops.iter().map(view_change_key).collect();
    // Reverse edges: which live views select from each view, keyed by the dependency's effective
    // `(schema, name)` (an unqualified source resolves to its own view's schema).
    let mut dependents_of: BTreeMap<ViewKey, Vec<ViewKey>> = BTreeMap::new();
    for (key, view) in &actual_views {
        for source in view.referenced_sources() {
            let dependency = (
                source.schema.clone().or_else(|| key.0.clone()),
                source.name.clone(),
            );
            dependents_of
                .entry(dependency)
                .or_default()
                .push(key.clone());
        }
    }
    let mut worklist: Vec<ViewKey> = dropped.iter().cloned().collect();
    while let Some(key) = worklist.pop() {
        let Some(dependents) = dependents_of.get(&key) else {
            continue;
        };
        for dependent in dependents.clone() {
            if !dropped.insert(dependent.clone()) {
                continue;
            }
            worklist.push(dependent.clone());
            if let Some(actual_view) = actual_views.get(&dependent) {
                drops.push(DatabaseDiffChange::DropView {
                    schema: dependent.0.clone(),
                    view: (*actual_view).clone(),
                });
            }
            // Recreate the dependent from the desired model if it still exists and a recreate is not
            // already queued (e.g. from the conservative `CREATE OR REPLACE` of an introspected view).
            if let Some(desired_view) = desired_views.get(&dependent)
                && !creates
                    .iter()
                    .any(|change| view_change_key(change) == dependent)
            {
                creates.push(DatabaseDiffChange::CreateView {
                    schema: dependent.0.clone(),
                    view: (*desired_view).clone(),
                });
            }
        }
    }

    // Order creates dependencies-first (a view after every other view it selects from) and drops
    // dependents-first, spanning schemas, so a view-on-view never references a sibling that does not
    // exist yet (create) or has already been removed (drop).
    let desired_order = view_dependency_order(&desired_views);
    let actual_order = view_dependency_order(&actual_views);
    creates.sort_by_key(|change| dependency_rank(&desired_order, &view_change_key(change)));
    drops.sort_by_key(|change| {
        std::cmp::Reverse(dependency_rank(&actual_order, &view_change_key(change)))
    });
    (drops, creates)
}

fn view_columns_differ_ignoring_nullability(
    desired: &[ViewColumnModel],
    actual: &[ViewColumnModel],
) -> bool {
    desired.len() != actual.len()
        || desired
            .iter()
            .zip(actual)
            .any(|(desired, actual)| desired.name != actual.name || desired.ty != actual.ty)
}

/// The view's name from a `CreateView`/`DropView` change.
fn dependency_rank(order: &[ViewKey], key: &ViewKey) -> usize {
    order
        .iter()
        .position(|candidate| candidate == key)
        .unwrap_or(usize::MAX)
}

/// Returns every view's key in dependency order — a view appears after every other view it selects
/// from (resolving an unqualified source to its own schema). A depth-first post-order; reference
/// cycles (which SQL rejects) fall back to map order.
fn view_dependency_order(views: &BTreeMap<ViewKey, &ViewModel>) -> Vec<ViewKey> {
    fn visit(
        index: usize,
        keys: &[ViewKey],
        views: &BTreeMap<ViewKey, &ViewModel>,
        visited: &mut [bool],
        order: &mut Vec<ViewKey>,
    ) {
        if visited[index] {
            return;
        }
        visited[index] = true;
        let (schema, _) = &keys[index];
        for source in views[&keys[index]].referenced_sources() {
            let dependency = (
                source.schema.clone().or_else(|| schema.clone()),
                source.name.clone(),
            );
            if let Some(dependency_index) = keys.iter().position(|key| *key == dependency)
                && dependency_index != index
            {
                visit(dependency_index, keys, views, visited, order);
            }
        }
        order.push(keys[index].clone());
    }

    let keys: Vec<ViewKey> = views.keys().cloned().collect();
    let mut order = Vec::with_capacity(keys.len());
    let mut visited = vec![false; keys.len()];
    for index in 0..keys.len() {
        visit(index, &keys, views, &mut visited, &mut order);
    }
    order
}

pub(crate) fn diff_table(desired: &TableModel, actual: &TableModel) -> Vec<TableDiffChange> {
    let mut changes = Vec::new();

    if desired.comment != actual.comment {
        changes.push(TableDiffChange::SetTableComment {
            before: actual.comment.clone(),
            after: desired.comment.clone(),
        });
    }

    diff_named_vec(
        &desired.columns,
        &actual.columns,
        |column| column.name.clone(),
        |column| TableDiffChange::AddColumn {
            column: column.clone(),
        },
        |column| TableDiffChange::DropColumn {
            column: column.clone(),
        },
        |before, after| TableDiffChange::AlterColumn {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    diff_primary_key(&desired.primary_key, &actual.primary_key, &mut changes);

    diff_named_vec(
        &desired.uniques,
        &actual.uniques,
        |constraint| constraint.name.clone(),
        |constraint| TableDiffChange::AddUnique {
            constraint: constraint.clone(),
        },
        |constraint| TableDiffChange::DropUnique {
            constraint: constraint.clone(),
        },
        |before, after| TableDiffChange::AlterUnique {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    diff_named_vec(
        &desired.foreign_keys,
        &actual.foreign_keys,
        |foreign_key| foreign_key.name.clone(),
        |foreign_key| TableDiffChange::AddForeignKey {
            foreign_key: foreign_key.clone(),
        },
        |foreign_key| TableDiffChange::DropForeignKey {
            foreign_key: foreign_key.clone(),
        },
        |before, after| TableDiffChange::AlterForeignKey {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    diff_named_vec(
        &desired.checks,
        &actual.checks,
        |check| check.name.clone(),
        |check| TableDiffChange::AddCheck {
            check: check.clone(),
        },
        |check| TableDiffChange::DropCheck {
            check: check.clone(),
        },
        |before, after| TableDiffChange::AlterCheck {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    diff_named_vec(
        &desired.indexes,
        &actual.indexes,
        |index| index.name.clone(),
        |index| TableDiffChange::AddIndex {
            index: index.clone(),
        },
        |index| TableDiffChange::DropIndex {
            index: index.clone(),
        },
        |before, after| TableDiffChange::AlterIndex {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    changes
}

fn diff_primary_key(
    desired: &Option<Constraint>,
    actual: &Option<Constraint>,
    changes: &mut Vec<TableDiffChange>,
) {
    match (desired, actual) {
        (Some(desired), None) => changes.push(TableDiffChange::AddPrimaryKey {
            constraint: desired.clone(),
        }),
        (None, Some(actual)) => changes.push(TableDiffChange::DropPrimaryKey {
            constraint: actual.clone(),
        }),
        (Some(desired), Some(actual)) if desired != actual => {
            changes.push(TableDiffChange::AlterPrimaryKey {
                before: actual.clone(),
                after: desired.clone(),
            });
        }
        _ => {}
    }
}

fn diff_named_vec<T, Key, Add, Drop, Alter>(
    desired: &[T],
    actual: &[T],
    key: Key,
    add: Add,
    drop: Drop,
    alter: Alter,
    changes: &mut Vec<TableDiffChange>,
) where
    T: PartialEq,
    Key: Fn(&T) -> String,
    Add: Fn(&T) -> TableDiffChange,
    Drop: Fn(&T) -> TableDiffChange,
    Alter: Fn(&T, &T) -> TableDiffChange,
{
    let desired = keyed(desired, &key);
    let actual = keyed(actual, &key);

    for item_key in sorted_keys(&desired, &actual) {
        match (desired.get(&item_key), actual.get(&item_key)) {
            (Some(desired_item), None) => changes.push(add(desired_item)),
            (None, Some(actual_item)) => changes.push(drop(actual_item)),
            (Some(desired_item), Some(actual_item)) if *desired_item != *actual_item => {
                changes.push(alter(actual_item, desired_item));
            }
            _ => {}
        }
    }
}

fn keyed_schemas(schemas: &[SchemaModel]) -> BTreeMap<Option<String>, &SchemaModel> {
    schemas
        .iter()
        .map(|schema| (schema.name.clone(), schema))
        .collect()
}

fn keyed_tables(tables: &[TableModel]) -> BTreeMap<String, &TableModel> {
    tables
        .iter()
        .map(|table| (table.name.clone(), table))
        .collect()
}

fn keyed<'a, T, Key>(items: &'a [T], key: &Key) -> BTreeMap<String, &'a T>
where
    Key: Fn(&T) -> String,
{
    items.iter().map(|item| (key(item), item)).collect()
}

fn sorted_keys<K, V>(left: &BTreeMap<K, V>, right: &BTreeMap<K, V>) -> Vec<K>
where
    K: Clone + Ord,
{
    left.keys()
        .chain(right.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
