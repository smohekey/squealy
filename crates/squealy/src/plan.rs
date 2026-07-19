use crate::{
    CheckModel, ColumnModel, Constraint, DomainModel, EnumModel, ExclusionModel, ForeignKeyModel,
    IndexModel, SequenceModel, SequenceOwnedBy, SqlType, TableModel, ViewModel,
};

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
    CreateView {
        schema: Option<String>,
        // Boxed like the table payloads: `ViewModel` carries the structural body and column list.
        view: Box<ViewModel>,
    },
    DropView {
        schema: Option<String>,
        view: Box<ViewModel>,
    },
    /// Create an enum type (`CREATE TYPE ... AS ENUM`). Ordered before any table that uses it.
    CreateEnum {
        schema: Option<String>,
        enum_type: EnumModel,
    },
    /// Drop an enum type. Ordered after any table that used it is gone.
    DropEnum {
        schema: Option<String>,
        enum_type: EnumModel,
    },
    /// Change an enum's labels. `additive` (the actual labels are a prefix of the desired) is an in-place
    /// `ALTER TYPE ... ADD VALUE`; otherwise the type is recreated and every column of the type rewritten
    /// (the renderer finds those columns in the desired model it is given).
    AlterEnum {
        schema: Option<String>,
        before: EnumModel,
        after: EnumModel,
        additive: bool,
    },
    /// Create a sequence (`CREATE SEQUENCE`), without its `OWNED BY` clause. Ordered before any table
    /// whose column default `nextval`s it.
    CreateSequence {
        schema: Option<String>,
        sequence: SequenceModel,
    },
    /// Drop a sequence. Ordered after any table whose column default referenced it is gone.
    DropSequence {
        schema: Option<String>,
        sequence: SequenceModel,
    },
    /// Change a sequence's attributes in place (`ALTER SEQUENCE`). Ownership is a separate step.
    AlterSequence {
        schema: Option<String>,
        before: SequenceModel,
        after: SequenceModel,
    },
    /// Set or clear a sequence's owning column (`ALTER SEQUENCE ... OWNED BY`). Ordered after tables so
    /// the owning column exists.
    SetSequenceOwner {
        schema: Option<String>,
        name: String,
        owned_by: Option<SequenceOwnedBy>,
    },
    /// Create a domain type (`CREATE DOMAIN`). Ordered before any table whose column is of the domain.
    CreateDomain {
        schema: Option<String>,
        domain: DomainModel,
    },
    /// Drop a domain type. Ordered after any table that used it is gone.
    DropDomain {
        schema: Option<String>,
        domain: DomainModel,
    },
    /// Change a domain in place (`ALTER DOMAIN`): its `NOT NULL`, `DEFAULT`, and `CHECK` constraints.
    AlterDomain {
        schema: Option<String>,
        before: DomainModel,
        after: DomainModel,
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
        /// The renamed column's type. A backend may need it to rename a type-specific generated
        /// constraint alongside the column — PostgreSQL renames the generated `FixedBytes` length
        /// check (named from the column), which it does not rename automatically.
        column_type: SqlType,
    },
    AlterColumn {
        before: ColumnModel,
        after: ColumnModel,
        /// Optional `USING <expr>` cast for a type change, supplied by a `cast-column` refactor hint.
        /// Backends that support it (PostgreSQL) emit it on the `ALTER COLUMN ... TYPE` statement.
        type_cast: Option<String>,
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
    AddExclusion {
        exclusion: ExclusionModel,
    },
    DropExclusion {
        exclusion: ExclusionModel,
    },
    AlterExclusion {
        before: ExclusionModel,
        after: ExclusionModel,
    },
}
