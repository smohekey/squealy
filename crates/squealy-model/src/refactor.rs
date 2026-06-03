//! Explicit schema refactor operations.
//!
//! Refactors capture deployment intent that cannot be inferred safely from two schema snapshots.
//! For example, a removed column plus an added column may be a data-preserving rename or may be a
//! real drop/add. The schema model remains the current truth; this log records intentional
//! transitions between truths.

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
