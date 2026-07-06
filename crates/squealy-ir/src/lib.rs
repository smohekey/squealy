//! Backend-neutral schema and expression data types shared across the squealy crates.
//!
//! These are the pure, dependency-free data types of the owned schema model and the structural view
//! expression tree. They live in this leaf crate (with no workspace dependencies) so that both the
//! core `squealy` crate and tooling like `squealy-parse` can share them without a dependency cycle.
//! The compile-time query-builder types, the `#[derive]` machinery, and the `&dyn` walkers that
//! materialize these types all live in the core `squealy` crate.

// ===== expression operator / function data types =====

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithmeticOp {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Equals,
    NotEquals,
    LessThan,
    LessThanOrEquals,
    GreaterThan,
    GreaterThanOrEquals,
}

/// A SQL aggregate function applied to a single expression operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderDirection {
    Asc,
    Desc,
}

/// A date/time field for `extract` (rendered as the `EXTRACT` keyword, e.g. `YEAR`) and
/// `date_trunc` (rendered as the quoted unit literal, e.g. `'year'`). Each field's `i64` result is
/// uniform across PostgreSQL and MySQL. `Second` is the whole-seconds component (`0`–`59`): PostgreSQL's
/// `EXTRACT(SECOND …)` is fractional, so `extract` floors it to match MySQL's integer value — use
/// `extract_second` for the fractional part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl DateField {
    /// The `EXTRACT(<keyword> FROM …)` field keyword.
    pub fn extract_keyword(self) -> &'static str {
        match self {
            DateField::Year => "YEAR",
            DateField::Month => "MONTH",
            DateField::Day => "DAY",
            DateField::Hour => "HOUR",
            DateField::Minute => "MINUTE",
            DateField::Second => "SECOND",
        }
    }

    /// The `date_trunc('<literal>', …)` unit literal.
    pub fn trunc_literal(self) -> &'static str {
        match self {
            DateField::Year => "year",
            DateField::Month => "month",
            DateField::Day => "day",
            DateField::Hour => "hour",
            DateField::Minute => "minute",
            DateField::Second => "second",
        }
    }
}

/// The function part of a window expression (`func(args) OVER (…)`): a SQL aggregate used as a
/// window, or a dedicated window function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFunc {
    /// An aggregate (`SUM`/`AVG`/`COUNT`/`MIN`/`MAX`) used as a window function.
    Aggregate(AggregateFunc),
    RowNumber,
    Rank,
    DenseRank,
    Ntile,
    Lag,
    Lead,
}

/// The mode of a window frame: `ROWS` (physical, row-count offsets) or `RANGE` (logical, value
/// offsets relative to the `ORDER BY` key). Chosen by the `Window::rows` / `Window::range`
/// builder method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameMode {
    Rows,
    Range,
}

/// The stored value of a window frame bound (the left/right of `BETWEEN <start> AND <end>`). Offsets
/// are compile-time literals, so a frame contributes no runtime bind parameters. End users do not name
/// this directly — they build bounds with the typed constructors (`unbounded_preceding`,
/// `preceding`, `current_row`, `following`, `unbounded_following`), which the `FrameStart` /
/// `FrameEnd` traits restrict to the valid side. It is public for the view-model (de)serializer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameBound {
    /// `UNBOUNDED PRECEDING` — the start of the partition.
    UnboundedPreceding,
    /// `<n> PRECEDING` — `n` rows (or `n` of the order key's value) before the current row.
    Preceding(u64),
    /// `CURRENT ROW`.
    CurrentRow,
    /// `<n> FOLLOWING` — `n` rows (or value) after the current row.
    Following(u64),
    /// `UNBOUNDED FOLLOWING` — the end of the partition.
    UnboundedFollowing,
}

impl FrameBound {
    fn render<W: std::io::Write + ?Sized>(self, w: &mut W) -> std::io::Result<()> {
        match self {
            FrameBound::UnboundedPreceding => w.write_all(b"UNBOUNDED PRECEDING"),
            FrameBound::Preceding(n) => write!(w, "{n} PRECEDING"),
            FrameBound::CurrentRow => w.write_all(b"CURRENT ROW"),
            FrameBound::Following(n) => write!(w, "{n} FOLLOWING"),
            FrameBound::UnboundedFollowing => w.write_all(b"UNBOUNDED FOLLOWING"),
        }
    }
}

/// A concrete window frame clause (`{ROWS|RANGE} BETWEEN <start> AND <end>`) stored in a [`Window`]
/// and its [`WindowExprAst`]. Contributes no runtime params (the bounds are literals). PostgreSQL and
/// MySQL 8.0+ share this syntax, so it renders identically across backends.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSpec {
    mode: FrameMode,
    start: FrameBound,
    end: FrameBound,
}

impl FrameSpec {
    /// Construct a frame clause from its mode and bounds. Used by the view-model (de)serializer to
    /// rebuild a frame; query builders go through `Window::rows` / `Window::range` instead.
    pub fn new(mode: FrameMode, start: FrameBound, end: FrameBound) -> Self {
        Self { mode, start, end }
    }

    /// The frame mode (`ROWS` or `RANGE`).
    pub fn mode(&self) -> FrameMode {
        self.mode
    }

    /// The frame's start bound (the left of `BETWEEN … AND …`).
    pub fn start(&self) -> FrameBound {
        self.start
    }

    /// The frame's end bound (the right of `BETWEEN … AND …`).
    pub fn end(&self) -> FrameBound {
        self.end
    }

    /// Render the frame clause without a leading space (the caller emits the separator), e.g.
    /// `ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`. Shared by every backend renderer.
    pub fn render<W: std::io::Write + ?Sized>(&self, w: &mut W) -> std::io::Result<()> {
        w.write_all(match self.mode {
            FrameMode::Rows => b"ROWS BETWEEN ".as_slice(),
            FrameMode::Range => b"RANGE BETWEEN ".as_slice(),
        })?;
        self.start.render(w)?;
        w.write_all(b" AND ")?;
        self.end.render(w)
    }
}

// ===== owned, backend-neutral schema model =====

/// A namespace within a database (a SQL "schema").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SchemaModel {
    /// The namespace name, or `None` for the default/unqualified namespace.
    pub name: Option<String>,
    pub tables: Vec<TableModel>,
    /// Views declared in this namespace. A view is a named `SELECT` with a typed output schema and a
    /// backend-neutral structural body; see [`ViewModel`].
    pub views: Vec<ViewModel>,
}

/// A table and its table-level, named constraints.
///
/// Unlike the query-side `Column` trait (which hangs primary-key/unique/foreign-key/check facts off
/// each column), the model hoists those into named table-level lists. This matches `ALTER TABLE … ADD
/// CONSTRAINT`, how catalogs report constraints during introspection, and admits composite keys.
#[derive(Clone, Debug, PartialEq)]
pub struct TableModel {
    pub name: String,
    pub comment: Option<String>,
    pub columns: Vec<ColumnModel>,
    pub primary_key: Option<Constraint>,
    pub foreign_keys: Vec<ForeignKeyModel>,
    pub uniques: Vec<Constraint>,
    pub checks: Vec<CheckModel>,
    pub indexes: Vec<IndexModel>,
}

/// Per-column facts (the table-level constraints live on [`TableModel`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnModel {
    pub name: String,
    pub comment: Option<String>,
    pub ty: SqlType,
    pub collation: Option<String>,
    pub nullable: bool,
    pub default: Option<DefaultValue>,
    pub identity: Option<IdentityModel>,
    pub generated: Option<GeneratedColumnModel>,
}

/// Backend-neutral identity / auto-increment metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityModel {
    pub mode: IdentityMode,
}

/// How a backend should generate identity values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityMode {
    /// PostgreSQL-style `GENERATED ALWAYS AS IDENTITY`.
    Always,
    /// PostgreSQL-style `GENERATED BY DEFAULT AS IDENTITY`.
    ByDefault,
    /// MySQL-style `AUTO_INCREMENT`.
    AutoIncrement,
}

/// Backend-neutral generated-column metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedColumnModel {
    pub expression: String,
    pub storage: GeneratedStorage,
}

/// Storage mode for a generated column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GeneratedStorage {
    Virtual,
    Stored,
    Unknown,
}

/// The owned, backend-neutral logical column type.
///
/// This mirrors the compile-time `ColumnType` but owns its strings, so a model can be rebuilt from
/// a package or live-database introspection (the `const`-friendly `ColumnType` borrows `'static`
/// strings and cannot represent runtime-sourced values). It is the place the neutral type vocabulary
/// grows structurally (e.g. `Varchar { len }`) as introspection lands.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlType {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    F32,
    F64,
    String,
    Bool,
    Varchar(u32),
    Char(u32),
    Text,
    Decimal {
        precision: u32,
        scale: u32,
    },
    Date,
    Time {
        tz: bool,
        /// Fractional-seconds precision (`TIME(n)`). `None` renders the bare, backend-default form.
        precision: Option<u8>,
    },
    Timestamp {
        tz: bool,
        /// Fractional-seconds precision (`TIMESTAMP(n)`). `None` renders the bare, backend-default form
        /// (MySQL fsp 0, PostgreSQL microseconds).
        precision: Option<u8>,
    },
    Uuid,
    Json,
    Jsonb,
    Bytes,
    /// A fixed-width binary column of `N` bytes (`[u8; N]`): PostgreSQL `bytea` + a generated
    /// `CHECK (octet_length(col) = N)`; MySQL `BINARY(N)`.
    FixedBytes(u32),
    /// A backend-specific type name, emitted verbatim into DDL.
    Raw(String),
}

/// The owned, backend-neutral mirror of `ColumnDefault` (owns its strings; see [`SqlType`]).
#[derive(Clone, Debug, PartialEq)]
pub enum DefaultValue {
    Null,
    Int(i128),
    UInt(u128),
    Float(f64),
    Text(String),
    Bool(bool),
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
    /// A backend-specific default expression, emitted verbatim into DDL.
    Raw(String),
}

/// A named constraint over one or more columns (primary key, unique).
#[derive(Clone, Debug, PartialEq)]
pub struct Constraint {
    pub name: String,
    pub columns: Vec<String>,
}

/// A named foreign-key constraint.
#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKeyModel {
    pub name: String,
    pub columns: Vec<String>,
    pub references_schema: Option<String>,
    pub references_table: String,
    pub references_columns: Vec<String>,
    pub match_type: Option<ForeignKeyMatch>,
    pub deferrability: Option<ConstraintDeferrability>,
    pub validation: Option<ConstraintValidation>,
    pub enforcement: Option<ConstraintEnforcement>,
    pub on_delete: Option<ForeignKeyAction>,
    pub on_update: Option<ForeignKeyAction>,
}

/// Whether a constraint has been validated against existing data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstraintValidation {
    Validated,
    NotValidated,
    /// A backend-specific validation state.
    Raw(String),
}

impl ConstraintValidation {
    pub fn from_sql(validation: &str) -> Self {
        let normalized = validation
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], " ");
        match normalized.as_str() {
            "validated" | "valid" => ConstraintValidation::Validated,
            "not validated" | "not valid" => ConstraintValidation::NotValidated,
            _ => ConstraintValidation::Raw(validation.to_owned()),
        }
    }

    pub fn as_sql(&self) -> &str {
        match self {
            ConstraintValidation::Validated => "VALID",
            ConstraintValidation::NotValidated => "NOT VALID",
            ConstraintValidation::Raw(validation) => validation,
        }
    }
}

/// Whether a constraint is actively enforced for writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstraintEnforcement {
    Enforced,
    NotEnforced,
    /// A backend-specific enforcement state.
    Raw(String),
}

impl ConstraintEnforcement {
    pub fn from_sql(enforcement: &str) -> Self {
        let normalized = enforcement
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], " ");
        match normalized.as_str() {
            "enforced" => ConstraintEnforcement::Enforced,
            "not enforced" => ConstraintEnforcement::NotEnforced,
            _ => ConstraintEnforcement::Raw(enforcement.to_owned()),
        }
    }

    pub fn as_sql(&self) -> &str {
        match self {
            ConstraintEnforcement::Enforced => "ENFORCED",
            ConstraintEnforcement::NotEnforced => "NOT ENFORCED",
            ConstraintEnforcement::Raw(enforcement) => enforcement,
        }
    }
}

/// Backend-neutral constraint deferrability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstraintDeferrability {
    InitiallyImmediate,
    InitiallyDeferred,
    /// A backend-specific deferrability clause, emitted verbatim into DDL.
    Raw(String),
}

impl ConstraintDeferrability {
    pub fn from_sql(deferrability: &str) -> Self {
        let normalized = deferrability
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], " ");
        match normalized.as_str() {
            "initially immediate" | "deferrable initially immediate" => {
                ConstraintDeferrability::InitiallyImmediate
            }
            "initially deferred" | "deferrable initially deferred" => {
                ConstraintDeferrability::InitiallyDeferred
            }
            _ => ConstraintDeferrability::Raw(deferrability.to_owned()),
        }
    }

    pub fn as_sql(&self) -> &str {
        match self {
            ConstraintDeferrability::InitiallyImmediate => "DEFERRABLE INITIALLY IMMEDIATE",
            ConstraintDeferrability::InitiallyDeferred => "DEFERRABLE INITIALLY DEFERRED",
            ConstraintDeferrability::Raw(deferrability) => deferrability,
        }
    }
}

/// Backend-neutral foreign-key match type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForeignKeyMatch {
    Simple,
    Partial,
    Full,
    /// A backend-specific match type, emitted verbatim into DDL.
    Raw(String),
}

impl ForeignKeyMatch {
    pub fn from_sql(match_type: &str) -> Self {
        match match_type.trim().to_ascii_lowercase().as_str() {
            "simple" => ForeignKeyMatch::Simple,
            "partial" => ForeignKeyMatch::Partial,
            "full" => ForeignKeyMatch::Full,
            _ => ForeignKeyMatch::Raw(match_type.to_owned()),
        }
    }

    pub fn as_sql(&self) -> &str {
        match self {
            ForeignKeyMatch::Simple => "SIMPLE",
            ForeignKeyMatch::Partial => "PARTIAL",
            ForeignKeyMatch::Full => "FULL",
            ForeignKeyMatch::Raw(match_type) => match_type,
        }
    }
}

/// Backend-neutral referential action for a foreign key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForeignKeyAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
    /// A backend-specific action, emitted verbatim into DDL.
    Raw(String),
}

impl ForeignKeyAction {
    pub fn from_sql(action: &str) -> Self {
        let normalized = action.trim().to_ascii_lowercase().replace(['-', '_'], " ");
        match normalized.as_str() {
            "no action" => ForeignKeyAction::NoAction,
            "restrict" => ForeignKeyAction::Restrict,
            "cascade" => ForeignKeyAction::Cascade,
            "set null" => ForeignKeyAction::SetNull,
            "set default" => ForeignKeyAction::SetDefault,
            _ => ForeignKeyAction::Raw(action.to_owned()),
        }
    }

    pub fn as_sql(&self) -> &str {
        match self {
            ForeignKeyAction::NoAction => "NO ACTION",
            ForeignKeyAction::Restrict => "RESTRICT",
            ForeignKeyAction::Cascade => "CASCADE",
            ForeignKeyAction::SetNull => "SET NULL",
            ForeignKeyAction::SetDefault => "SET DEFAULT",
            ForeignKeyAction::Raw(action) => action,
        }
    }
}

/// A named check constraint carrying a backend-specific boolean expression.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckModel {
    pub name: String,
    pub expression: String,
    pub validation: Option<ConstraintValidation>,
    pub enforcement: Option<ConstraintEnforcement>,
}

/// A named index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexModel {
    pub name: String,
    /// Quoted column terms in the index key.
    pub columns: Vec<String>,
    /// Backend-specific expression terms in the index key, emitted verbatim.
    pub expressions: Vec<String>,
    /// Non-key columns stored with a covering index.
    pub include_columns: Vec<String>,
    pub unique: bool,
    pub method: Option<IndexMethod>,
    pub directions: Vec<IndexDirection>,
    pub nulls: Vec<IndexNullsOrder>,
    /// Backend-specific collations by zero-based key-term position.
    pub collations: Vec<IndexCollation>,
    /// Backend-specific operator classes by zero-based key-term position.
    pub operator_classes: Vec<IndexOperatorClass>,
    /// Backend-specific predicate for a partial index.
    pub predicate: Option<String>,
}

/// Sort direction for an indexed column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexDirection {
    Asc,
    Desc,
}

/// Null ordering for an indexed key term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexNullsOrder {
    First,
    Last,
}

/// Collation override for an indexed key term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCollation {
    pub position: usize,
    pub name: String,
}

/// Operator class override for an indexed key term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexOperatorClass {
    pub position: usize,
    pub name: String,
}

/// Backend-neutral index access method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexMethod {
    BTree,
    Hash,
    Gin,
    Gist,
    SpGist,
    Brin,
    /// A backend-specific index method, emitted verbatim into DDL.
    Raw(String),
}

impl IndexMethod {
    pub fn from_sql(method: &str) -> Self {
        match method.trim().to_ascii_lowercase().as_str() {
            "btree" => IndexMethod::BTree,
            "hash" => IndexMethod::Hash,
            "gin" => IndexMethod::Gin,
            "gist" => IndexMethod::Gist,
            "spgist" => IndexMethod::SpGist,
            "brin" => IndexMethod::Brin,
            _ => IndexMethod::Raw(method.to_owned()),
        }
    }

    pub fn postgres_sql(&self) -> &str {
        match self {
            IndexMethod::BTree => "btree",
            IndexMethod::Hash => "hash",
            IndexMethod::Gin => "gin",
            IndexMethod::Gist => "gist",
            IndexMethod::SpGist => "spgist",
            IndexMethod::Brin => "brin",
            IndexMethod::Raw(method) => method,
        }
    }

    pub fn mysql_sql(&self) -> &str {
        match self {
            IndexMethod::BTree => "BTREE",
            IndexMethod::Hash => "HASH",
            IndexMethod::Gin => "GIN",
            IndexMethod::Gist => "GIST",
            IndexMethod::SpGist => "SPGIST",
            IndexMethod::Brin => "BRIN",
            IndexMethod::Raw(method) => method,
        }
    }
}

/// A view: a named `SELECT` with a typed output schema and a backend-neutral structural body.
///
/// The compile-time `#[derive(View)]` type is the source of truth; the walker materializes it here so
/// the same DDL-management operations (render, package export/import, future diff) that consume
/// [`TableModel`] also consume views. The body is structural ([`ViewQueryModel`]) so each backend
/// renders its own dialect and the model round-trips through a package.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewModel {
    pub name: String,
    pub comment: Option<String>,
    /// The view's output columns, in projection order. Powers DDL (the optional column list),
    /// packaging, introspection, and the queryable typed face.
    pub columns: Vec<ViewColumnModel>,
    /// The structural body of the view's `SELECT`.
    pub query: ViewQueryModel,
}

/// One output column of a [`ViewModel`].
#[derive(Clone, Debug, PartialEq)]
pub struct ViewColumnModel {
    pub name: String,
    pub ty: SqlType,
    pub nullable: bool,
}

/// The backend-neutral structural body of a view's `SELECT`.
///
/// Source/join/projection structure and the inner scalar expressions (predicate bodies, projection
/// expressions, group/having/order keys) are all captured structurally as [`ExprNode`] trees, so each
/// backend renders them in its own dialect. Literals are inlined (a view body carries no bind
/// parameters).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewQueryModel {
    /// Whether the view body is `SELECT DISTINCT`.
    pub distinct: bool,
    pub projection: Vec<ProjectionItem>,
    pub from: Option<SourceRef>,
    pub joins: Vec<JoinItem>,
    pub filter: Option<ExprNode>,
    pub group_by: Vec<ExprNode>,
    pub having: Option<ExprNode>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// View-on-view dependencies recorded by live introspection, which cannot reconstruct the body
    /// into the structural form above. Empty for declared/package views, whose dependencies are found
    /// by walking the body; [`ViewModel::referenced_sources`] folds these in so live drop ordering
    /// still sees view-on-view edges. Not serialized (introspected models are never packaged).
    pub dependencies: Vec<SourceRef>,
}

/// One projected output expression together with its output column name.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectionItem {
    pub output_name: String,
    pub expr: ExprNode,
}

/// A table or view referenced in a view body, with the alias bound to it in the `SELECT`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: String,
}

/// A join in a view body.
#[derive(Clone, Debug, PartialEq)]
pub struct JoinItem {
    pub kind: JoinKind,
    pub source: SourceRef,
    /// The `ON` condition, or `None` for a `CROSS JOIN` (Cartesian product, no condition).
    pub on: Option<ExprNode>,
}

/// The kind of join in a view body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    /// `CROSS JOIN` — Cartesian product, no `ON` condition.
    Cross,
}

/// One `ORDER BY` term in a view body.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderItem {
    pub expr: ExprNode,
    pub direction: Option<OrderDirection>,
    pub nulls: Option<OrderNulls>,
}

/// Null ordering for an `ORDER BY` term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderNulls {
    First,
    Last,
}

/// A backend-neutral SQL expression from a view body.
///
/// Stored structurally (rather than as pre-rendered text) so each backend renders it in its own
/// dialect — correct identifier quoting, cast type names, integer-division casts, and `LIKE`/`ILIKE`.
/// Literals are inlined, since a view body carries no bind parameters.
#[derive(Clone, Debug, PartialEq)]
pub enum ExprNode {
    /// A qualified column reference, rendered as `<alias>.<column>`. A view body binds every column to
    /// a source alias; the unqualified form used by constraint / generated / index expressions is
    /// [`ExprNode::BareColumn`].
    Column { alias: String, column: String },
    /// An unqualified column reference, rendered as a bare `<column>`. Constraint, generated-column, and
    /// index expressions name columns of their own table with no alias.
    BareColumn { column: String },
    /// An inlined SQL literal, already formatted (e.g. `'Ada'`, `42`, `TRUE`, `NULL`).
    Literal(String),
    /// Binary arithmetic; `Divide` uses the backend's fractional-division handling.
    Binary {
        op: ArithmeticOp,
        left: Box<ExprNode>,
        right: Box<ExprNode>,
    },
    /// `CAST(<operand> AS <ty>)`, with the backend's spelling of `ty`.
    Cast { operand: Box<ExprNode>, ty: SqlType },
    /// An aggregate call `FUNC([DISTINCT] <operand>)`, optionally wrapped in a cast to `result` so the
    /// output column's wire type matches the view's declared column type.
    Aggregate {
        func: AggregateFunc,
        distinct: bool,
        operand: Box<ExprNode>,
        result: Option<SqlType>,
    },
    /// A comparison `<left> <op> <right>`.
    Compare {
        op: CompareOp,
        left: Box<ExprNode>,
        right: Box<ExprNode>,
    },
    /// A logical `AND`/`OR`.
    Logical {
        op: LogicalOp,
        left: Box<ExprNode>,
        right: Box<ExprNode>,
    },
    /// `NOT (<operand>)`.
    Not(Box<ExprNode>),
    /// `<operand> IS [NOT] NULL`.
    IsNull {
        negated: bool,
        operand: Box<ExprNode>,
    },
    /// `<operand> [NOT] LIKE <pattern>` (`ILIKE` for `case_insensitive` on supporting dialects).
    Like {
        case_insensitive: bool,
        negated: bool,
        operand: Box<ExprNode>,
        pattern: Box<ExprNode>,
    },
    /// `<operand> [NOT] IN (<items>)` against an inline value list.
    In {
        negated: bool,
        operand: Box<ExprNode>,
        items: Vec<ExprNode>,
    },
    /// `<operand> [NOT] BETWEEN <low> AND <high>`.
    Between {
        negated: bool,
        operand: Box<ExprNode>,
        low: Box<ExprNode>,
        high: Box<ExprNode>,
    },
    /// A scalar subquery `(SELECT …)` used as a value.
    ScalarSubquery(Box<ViewQueryModel>),
    /// `<operand> [NOT] IN (<subquery>)`.
    InSubquery {
        negated: bool,
        operand: Box<ExprNode>,
        subquery: Box<ViewQueryModel>,
    },
    /// `[NOT] EXISTS (<subquery>)`.
    Exists {
        negated: bool,
        subquery: Box<ViewQueryModel>,
    },
    /// A window function: `FUNC(<args>) OVER (PARTITION BY … ORDER BY … <frame>)`, optionally cast to
    /// `result`. `frame` is the optional `ROWS`/`RANGE BETWEEN …` clause (literal bounds, no params).
    Window {
        func: WindowFunc,
        args: Vec<ExprNode>,
        partition_by: Vec<ExprNode>,
        order_by: Vec<WindowOrderTerm>,
        frame: Option<FrameSpec>,
        result: Option<SqlType>,
    },
    /// Searched `CASE WHEN … THEN … [ELSE …] END`, optionally wrapped in `CAST(… AS result)` to pin
    /// the result type (so all-parameter branches stay typeable).
    Case {
        arms: Vec<CaseArm>,
        else_: Option<Box<ExprNode>>,
        result: Option<SqlType>,
    },
    /// `NULLIF(<left>, <right>)`. Each operand is cast to `result` (when set) so all-parameter operands
    /// stay typeable; mirrors the per-branch CAST on [`ExprNode::Case`].
    Nullif {
        left: Box<ExprNode>,
        right: Box<ExprNode>,
        result: Option<SqlType>,
    },
    /// `COALESCE(<args>)`. Each argument is cast to `result` (when set) so an all-parameter `COALESCE`
    /// stays typeable.
    Coalesce {
        args: Vec<ExprNode>,
        result: Option<SqlType>,
    },
    /// Simple `CASE <operand> WHEN <value> THEN … [ELSE …] END` (each `CaseArm`'s `when` is the value
    /// compared against `operand`). The `THEN`/`ELSE` values are cast to `result` (when set).
    SimpleCase {
        operand: Box<ExprNode>,
        arms: Vec<CaseArm>,
        else_: Option<Box<ExprNode>>,
        result: Option<SqlType>,
    },
    /// A scalar string function call — `FUNC(<args>)` (the function names are identical across
    /// backends; no cast is needed since a function call self-types its result).
    ScalarFn {
        func: ScalarFunc,
        args: Vec<ExprNode>,
    },
    /// `CURRENT_TIMESTAMP`.
    Now,
    /// `CAST(EXTRACT(<field> FROM <operand>) AS <result>)` — `result` pins the dialect-divergent
    /// native `EXTRACT` type to a uniform type. `timezone` is `Some(tz)` for the timezone-explicit form
    /// (`<operand> AT TIME ZONE '<tz>'`, PostgreSQL only).
    Extract {
        field: DateField,
        operand: Box<ExprNode>,
        result: Option<SqlType>,
        timezone: Option<String>,
    },
    /// `date_trunc('<unit>', <operand>)` — PostgreSQL only; a MySQL view carrying it fails at DDL exec
    /// (like a `full_join` view). `timezone` is `Some(tz)` for the timezone-explicit form
    /// (`<operand> AT TIME ZONE '<tz>'`).
    DateTrunc {
        unit: DateField,
        operand: Box<ExprNode>,
        timezone: Option<String>,
    },
    /// Fractional seconds of a timestamp as `result` (`f64`). Dialect-divergent: PostgreSQL
    /// `EXTRACT(SECOND …)` vs MySQL `EXTRACT(SECOND_MICROSECOND …) / 1000000.0`.
    ExtractSecond {
        operand: Box<ExprNode>,
        result: Option<SqlType>,
    },
}

/// A scalar (string) function for [`ExprNode::ScalarFn`]. `Length` renders as `CHAR_LENGTH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarFunc {
    Lower,
    Upper,
    Length,
    Trim,
    Concat,
    Substring,
}

/// One `ORDER BY` term inside a window function's `OVER (…)` clause.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOrderTerm {
    pub expr: ExprNode,
    pub direction: OrderDirection,
}

/// One `WHEN <when> THEN <then>` arm of an [`ExprNode::Case`].
#[derive(Clone, Debug, PartialEq)]
pub struct CaseArm {
    pub when: Box<ExprNode>,
    pub then: Box<ExprNode>,
}

/// Conjunction/disjunction for [`ExprNode::Logical`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
}

impl ViewModel {
    /// The sources this view depends on, used to topologically order view creation. This walks the
    /// whole body — `FROM`/`JOIN`s and every expression, recursing into scalar/`IN`/`EXISTS`
    /// subqueries — so a view that references another view only inside a subquery is still ordered
    /// after it. Dependencies on tables are irrelevant to view ordering (all tables precede any view).
    pub fn referenced_sources(&self) -> impl Iterator<Item = &SourceRef> {
        let mut sources = Vec::new();
        collect_query_sources(&self.query, &mut sources);
        sources.into_iter()
    }
}

/// Collects every [`SourceRef`] reachable from a query body, recursing through subqueries.
fn collect_query_sources<'a>(query: &'a ViewQueryModel, sources: &mut Vec<&'a SourceRef>) {
    // Introspected views carry no body but record their dependencies here; declared/package views
    // leave this empty and contribute their sources by walking the body below.
    sources.extend(query.dependencies.iter());
    sources.extend(query.from.iter());
    for join in &query.joins {
        sources.push(&join.source);
        if let Some(on) = &join.on {
            collect_expr_sources(on, sources);
        }
    }
    for item in &query.projection {
        collect_expr_sources(&item.expr, sources);
    }
    if let Some(filter) = &query.filter {
        collect_expr_sources(filter, sources);
    }
    for expr in &query.group_by {
        collect_expr_sources(expr, sources);
    }
    if let Some(having) = &query.having {
        collect_expr_sources(having, sources);
    }
    for order in &query.order_by {
        collect_expr_sources(&order.expr, sources);
    }
}

/// Collects every [`SourceRef`] reachable from an expression, recursing through nested subqueries.
fn collect_expr_sources<'a>(expr: &'a ExprNode, sources: &mut Vec<&'a SourceRef>) {
    match expr {
        ExprNode::Column { .. } | ExprNode::BareColumn { .. } | ExprNode::Literal(_) => {}
        ExprNode::Binary { left, right, .. }
        | ExprNode::Compare { left, right, .. }
        | ExprNode::Logical { left, right, .. } => {
            collect_expr_sources(left, sources);
            collect_expr_sources(right, sources);
        }
        ExprNode::Cast { operand, .. } | ExprNode::Aggregate { operand, .. } => {
            collect_expr_sources(operand, sources);
        }
        ExprNode::Not(operand) | ExprNode::IsNull { operand, .. } => {
            collect_expr_sources(operand, sources);
        }
        ExprNode::Like {
            operand, pattern, ..
        } => {
            collect_expr_sources(operand, sources);
            collect_expr_sources(pattern, sources);
        }
        ExprNode::In { operand, items, .. } => {
            collect_expr_sources(operand, sources);
            for item in items {
                collect_expr_sources(item, sources);
            }
        }
        ExprNode::Between {
            operand, low, high, ..
        } => {
            collect_expr_sources(operand, sources);
            collect_expr_sources(low, sources);
            collect_expr_sources(high, sources);
        }
        ExprNode::ScalarSubquery(subquery) | ExprNode::Exists { subquery, .. } => {
            collect_query_sources(subquery, sources);
        }
        ExprNode::InSubquery {
            operand, subquery, ..
        } => {
            collect_expr_sources(operand, sources);
            collect_query_sources(subquery, sources);
        }
        ExprNode::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                collect_expr_sources(arg, sources);
            }
            for partition in partition_by {
                collect_expr_sources(partition, sources);
            }
            for order in order_by {
                collect_expr_sources(&order.expr, sources);
            }
        }
        ExprNode::Case { arms, else_, .. } => {
            for arm in arms {
                collect_expr_sources(&arm.when, sources);
                collect_expr_sources(&arm.then, sources);
            }
            if let Some(else_) = else_ {
                collect_expr_sources(else_, sources);
            }
        }
        ExprNode::Nullif { left, right, .. } => {
            collect_expr_sources(left, sources);
            collect_expr_sources(right, sources);
        }
        ExprNode::Coalesce { args, .. } => {
            for arg in args {
                collect_expr_sources(arg, sources);
            }
        }
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            ..
        } => {
            collect_expr_sources(operand, sources);
            for arm in arms {
                collect_expr_sources(&arm.when, sources);
                collect_expr_sources(&arm.then, sources);
            }
            if let Some(else_) = else_ {
                collect_expr_sources(else_, sources);
            }
        }
        ExprNode::ScalarFn { args, .. } => {
            for arg in args {
                collect_expr_sources(arg, sources);
            }
        }
        ExprNode::Now => {}
        ExprNode::Extract { operand, .. }
        | ExprNode::DateTrunc { operand, .. }
        | ExprNode::ExtractSecond { operand, .. } => {
            collect_expr_sources(operand, sources);
        }
    }
}
