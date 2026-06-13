use crate::{CheckModel, ColumnModel, Constraint, ForeignKeyModel, IndexModel, TableModel};

/// An ordered backend-neutral schema deployment plan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DatabasePlan {
    pub steps: Vec<DatabasePlanStep>,
}

impl DatabasePlan {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// One ordered backend-neutral schema deployment step.
#[derive(Clone, Debug, PartialEq)]
pub enum DatabasePlanStep {
    CreateSchema {
        schema: Option<String>,
    },
    DropSchema {
        schema: Option<String>,
    },
    CreateTable {
        schema: Option<String>,
        // Boxed to keep `DatabasePlanStep` small: `TableModel` is by far the heaviest payload,
        // so storing it inline would bloat every variant (clippy::large_enum_variant).
        table: Box<TableModel>,
    },
    DropTable {
        schema: Option<String>,
        table: Box<TableModel>,
    },
    RenameTable {
        refactor_id: Option<String>,
        schema: Option<String>,
        from: String,
        to: String,
    },
    AlterTable {
        schema: Option<String>,
        table: String,
        // Boxed for the same reason as the `TableModel` payloads above.
        change: Box<TablePlanStep>,
    },
}

/// One ordered backend-neutral table deployment step.
#[derive(Clone, Debug, PartialEq)]
pub enum TablePlanStep {
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
    RenameColumn {
        refactor_id: Option<String>,
        from: String,
        to: String,
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
