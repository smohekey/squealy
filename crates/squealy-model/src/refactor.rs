//! Explicit schema refactor operations.
//!
//! Refactors capture deployment intent that cannot be inferred safely from two schema snapshots.
//! For example, a removed column plus an added column may be a data-preserving rename or may be a
//! real drop/add. The schema model remains the current truth; this log records intentional
//! transitions between truths.

use std::collections::BTreeSet;

use squealy::DatabaseModel;

/// A persistent list of explicit schema refactor operations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefactorLog {
    pub operations: Vec<RefactorOperation>,
}

impl RefactorLog {
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

/// A single explicit schema refactor operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefactorOperation {
    RenameTable(RenameTable),
    RenameColumn(RenameColumn),
}

impl RefactorOperation {
    pub fn id(&self) -> &str {
        match self {
            RefactorOperation::RenameTable(operation) => &operation.id,
            RefactorOperation::RenameColumn(operation) => &operation.id,
        }
    }
}

/// A table rename in one schema namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameTable {
    /// Stable operation id. Backends can record this in their applied-refactor metadata later.
    pub id: String,
    pub schema: Option<String>,
    pub from: String,
    pub to: String,
}

/// A column rename within one table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameColumn {
    /// Stable operation id. Backends can record this in their applied-refactor metadata later.
    pub id: String,
    pub schema: Option<String>,
    pub table: String,
    pub from: String,
    pub to: String,
}

/// A recorded refactor id does not match the live schema state it claims to represent.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AppliedRefactorError {
    #[error(
        "recorded refactor `{id}` expects table {} to exist",
        qualified(schema, table)
    )]
    RenameTableTargetMissing {
        id: String,
        schema: Option<String>,
        table: String,
    },
    #[error(
        "recorded refactor `{id}` expects old table {} to be absent",
        qualified(schema, table)
    )]
    RenameTableSourceStillExists {
        id: String,
        schema: Option<String>,
        table: String,
    },
    #[error(
        "recorded refactor `{id}` expects table {} to exist",
        qualified(schema, table)
    )]
    RenameColumnTableMissing {
        id: String,
        schema: Option<String>,
        table: String,
    },
    #[error(
        "recorded refactor `{id}` expects column {}.{column} to exist",
        qualified(schema, table)
    )]
    RenameColumnTargetMissing {
        id: String,
        schema: Option<String>,
        table: String,
        column: String,
    },
    #[error(
        "recorded refactor `{id}` expects old column {}.{column} to be absent",
        qualified(schema, table)
    )]
    RenameColumnSourceStillExists {
        id: String,
        schema: Option<String>,
        table: String,
        column: String,
    },
}

/// Removes already-recorded refactors from `refactors` after validating their obvious final state.
pub fn pending_refactors(
    refactors: &RefactorLog,
    applied_ids: &[String],
    actual: &DatabaseModel,
) -> Result<RefactorLog, AppliedRefactorError> {
    let applied_ids = applied_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut operations = Vec::new();

    for operation in &refactors.operations {
        if applied_ids.contains(operation.id()) {
            validate_applied_refactor(operation, actual)?;
        } else {
            operations.push(operation.clone());
        }
    }

    Ok(RefactorLog { operations })
}

fn validate_applied_refactor(
    operation: &RefactorOperation,
    actual: &DatabaseModel,
) -> Result<(), AppliedRefactorError> {
    match operation {
        RefactorOperation::RenameTable(operation) => {
            if table(actual, &operation.schema, &operation.to).is_none() {
                return Err(AppliedRefactorError::RenameTableTargetMissing {
                    id: operation.id.clone(),
                    schema: operation.schema.clone(),
                    table: operation.to.clone(),
                });
            }
            if table(actual, &operation.schema, &operation.from).is_some() {
                return Err(AppliedRefactorError::RenameTableSourceStillExists {
                    id: operation.id.clone(),
                    schema: operation.schema.clone(),
                    table: operation.from.clone(),
                });
            }
        }
        RefactorOperation::RenameColumn(operation) => {
            let Some(table) = table(actual, &operation.schema, &operation.table) else {
                return Err(AppliedRefactorError::RenameColumnTableMissing {
                    id: operation.id.clone(),
                    schema: operation.schema.clone(),
                    table: operation.table.clone(),
                });
            };
            if table
                .columns
                .iter()
                .all(|column| column.name != operation.to)
            {
                return Err(AppliedRefactorError::RenameColumnTargetMissing {
                    id: operation.id.clone(),
                    schema: operation.schema.clone(),
                    table: operation.table.clone(),
                    column: operation.to.clone(),
                });
            }
            if table
                .columns
                .iter()
                .any(|column| column.name == operation.from)
            {
                return Err(AppliedRefactorError::RenameColumnSourceStillExists {
                    id: operation.id.clone(),
                    schema: operation.schema.clone(),
                    table: operation.table.clone(),
                    column: operation.from.clone(),
                });
            }
        }
    }

    Ok(())
}

fn table<'model>(
    model: &'model DatabaseModel,
    schema_name: &Option<String>,
    table_name: &str,
) -> Option<&'model squealy::TableModel> {
    model
        .schemas
        .iter()
        .find(|schema| schema.name == *schema_name)?
        .tables
        .iter()
        .find(|table| table.name == table_name)
}

fn qualified(schema: &Option<String>, name: &str) -> String {
    match schema {
        Some(schema) => format!("{schema}.{name}"),
        None => name.to_owned(),
    }
}
