//! Name-based diffing for owned schema models.
//!
//! This module compares a desired [`DatabaseModel`] with an actual [`DatabaseModel`] and reports the
//! structural changes needed to make the actual model match the desired model. It does not infer
//! renames or render SQL; those are later planning/rendering steps.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use squealy::{
    CheckModel, ColumnModel, Constraint, DatabaseModel, ForeignKeyModel, IndexModel, SchemaModel,
    TableModel,
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
#[derive(Clone, Debug, PartialEq)]
pub struct DiffPolicyError {
    pub blocked: Vec<ClassifiedDatabaseDiffChange>,
}

impl fmt::Display for DiffPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diff contains {} blocked change(s)",
            self.blocked.len()
        )
    }
}

impl std::error::Error for DiffPolicyError {}

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
}

impl DatabaseDiffChange {
    pub fn risk(&self) -> ChangeRisk {
        match self {
            DatabaseDiffChange::CreateSchema { .. } | DatabaseDiffChange::CreateTable { .. } => {
                ChangeRisk::Safe
            }
            DatabaseDiffChange::DropSchema { .. } | DatabaseDiffChange::DropTable { .. } => {
                ChangeRisk::Destructive
            }
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
            }
            (None, Some(actual_schema)) => {
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
                diff_schema(desired_schema, actual_schema, &mut changes);
            }
            (None, None) => {}
        }
    }

    DatabaseDiff { changes }
}

fn diff_schema(desired: &SchemaModel, actual: &SchemaModel, changes: &mut Vec<DatabaseDiffChange>) {
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

fn diff_table(desired: &TableModel, actual: &TableModel) -> Vec<TableDiffChange> {
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
