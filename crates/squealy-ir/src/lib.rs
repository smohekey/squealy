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
///
/// The `expression` is [`Option`] because a column can be *marked* generated without an authored
/// expression: the `#[column(generated)]` derive attribute is a bare flag with no expression syntax,
/// so a macro-built model carries `None` (and the renderer rejects it — a generated column has to have
/// an expression). A real defining expression arrives only from a KDL package or live introspection,
/// as `Some`. It is a structural [`ExprNode`] so the backend renders it in its own dialect and the diff
/// compares it structurally (mirroring [`CheckModel`]).
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedColumnModel {
    pub expression: Option<ExprNode>,
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

/// A named check constraint carrying its boolean expression as a structural [`ExprNode`], so each
/// backend renders it in its own dialect and the diff compares it structurally.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckModel {
    pub name: String,
    pub expression: ExprNode,
    pub validation: Option<ConstraintValidation>,
    pub enforcement: Option<ConstraintEnforcement>,
}

/// A named index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexModel {
    pub name: String,
    /// Quoted column terms in the index key.
    pub columns: Vec<String>,
    /// Structural expression terms in the index key, rendered per backend (an expression index).
    pub expressions: Vec<ExprNode>,
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
    /// Structural predicate for a partial index, rendered per backend (a partial-index `WHERE`). Boxed
    /// so an [`ExprNode`] (a large enum) does not bloat every `IndexModel` when the predicate is absent.
    pub predicate: Option<Box<ExprNode>>,
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
    /// The structural body of the view — a single `SELECT` or a set operation.
    pub query: ViewBody,
}

/// One output column of a [`ViewModel`].
#[derive(Clone, Debug, PartialEq)]
pub struct ViewColumnModel {
    pub name: String,
    pub ty: SqlType,
    pub nullable: bool,
}

/// The backend-neutral structural body of a view definition: a single `SELECT` ([`ViewQueryModel`]), a
/// set operation (`UNION`/`INTERSECT`/`EXCEPT`, optionally `ALL`) over two nested bodies, or a `WITH`
/// (common-table-expression) prelude wrapping any of these.
///
/// A view whose body live introspection cannot reconstruct is stored as an **empty** [`ViewBody::Select`]
/// (empty projection) carrying only its recorded `dependencies` — the diff's body-unknown sentinel (see
/// [`ViewBody::is_empty`]). A reconstructed body is compared structurally.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewBody {
    /// A single `SELECT`.
    Select(Box<ViewQueryModel>),
    /// A set operation over two bodies. An optional trailing `ORDER BY`/`LIMIT`/`OFFSET` applies to the
    /// whole set — SQL places it after the final arm, so it lives on the `Set` node, not the arms.
    Set {
        op: ViewSetOp,
        /// `true` for the `… ALL` variant (keeps duplicate rows).
        all: bool,
        left: Box<ViewBody>,
        right: Box<ViewBody>,
        order_by: Vec<OrderItem>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    /// A `WITH` (common-table-expression) prelude wrapping any inner body. Nests (a CTE body or the inner
    /// body may itself be a `With`), and can appear in a derived-table subquery.
    With {
        /// `true` for `WITH RECURSIVE` — a clause-level keyword covering the whole prelude. A CTE whose
        /// body actually self-references is detected structurally at render time (by that body's set arm
        /// referencing the CTE's own name); this flag only governs the emitted keyword.
        recursive: bool,
        ctes: Vec<CteModel>,
        body: Box<ViewBody>,
    },
}

/// One common-table expression declared in a [`ViewBody::With`] prelude: `<name> [(<columns>)] AS
/// (<body>)`. `columns` is the optional `WITH` column list (empty = none declared; the body's own
/// projection names the outputs). A **recursive** CTE's `body` is a [`ViewBody::Set`] whose recursive arm
/// references `name`.
#[derive(Clone, Debug, PartialEq)]
pub struct CteModel {
    pub name: String,
    pub columns: Vec<String>,
    pub body: ViewBody,
}

impl Default for ViewBody {
    /// An empty single `SELECT` — the body-unknown form an introspected, un-reconstructed view carries.
    fn default() -> Self {
        ViewBody::Select(Box::default())
    }
}

impl ViewBody {
    /// Whether this is the body-unknown sentinel: an empty `SELECT` (no projection), as stored for a
    /// live-introspected view whose body could not be reconstructed. A `Set` body is never empty.
    pub fn is_empty(&self) -> bool {
        matches!(self, ViewBody::Select(select) if select.projection.is_empty())
    }

    /// The view-on-view dependencies recorded on an introspected (un-reconstructed) body, or `&[]` for a
    /// reconstructed body (whose dependencies are found by walking it). Only an empty `Select` carries
    /// these — see [`ViewModel::referenced_sources`].
    pub fn dependencies(&self) -> &[SourceRef] {
        match self {
            ViewBody::Select(select) => &select.dependencies,
            ViewBody::Set { .. } | ViewBody::With { .. } => &[],
        }
    }
}

/// A set operator in a view body. The base operator only; the `ALL` modifier is the separate `all` flag
/// on [`ViewBody::Set`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewSetOp {
    Union,
    Intersect,
    Except,
}

/// The backend-neutral structural body of a single view `SELECT`.
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
    pub from: Option<SourceItem>,
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

/// A `FROM`/`JOIN` source in a view body: either a named table/view or a derived table (a
/// parenthesized subquery, `(SELECT …) AS alias`). Both bind an alias that qualified columns reference.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceItem {
    /// A named relation — `<schema>.<name> AS <alias>`.
    Named(SourceRef),
    /// A derived table — `(<subquery>) AS <alias>`. The subquery is a full [`ViewBody`] (it may itself be
    /// a set operation), boxed for the recursive type.
    Derived { query: Box<ViewBody>, alias: String },
}

impl SourceItem {
    /// The alias bound to this source, which qualified columns in the enclosing `SELECT` reference.
    pub fn alias(&self) -> &str {
        match self {
            SourceItem::Named(source) => &source.alias,
            SourceItem::Derived { alias, .. } => alias,
        }
    }
}

/// A join in a view body.
#[derive(Clone, Debug, PartialEq)]
pub struct JoinItem {
    pub kind: JoinKind,
    pub source: SourceItem,
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
    /// An un-modelable expression, carried as its already-rendered dialect SQL and re-emitted verbatim.
    /// The last-resort escape hatch: live introspection uses it when the reverse parser cannot yet lower
    /// a backend's deparse output into a structural node. It is dialect-specific (does not re-render
    /// across dialects) and never produced by the forward path (the derive macro / typed builders), so a
    /// squealy-published object round-trips structurally; a `Raw` on one side of a diff means the
    /// introspected form could not be structured and will not compare equal to a structural desired node.
    Raw(String),
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
    /// A general, dialect-neutral function call `<name>(<args>)` for functions outside the closed
    /// [`ScalarFn`](ExprNode::ScalarFn) set — user-defined and other built-in functions in CHECK /
    /// generated-column / index expressions (`jsonb_typeof(data) = 'object'`). The `name` is stored
    /// lowercased (matching the forward path's authored spelling and PostgreSQL's unquoted deparse) and
    /// re-emitted verbatim: unlike [`ScalarFn`](ExprNode::ScalarFn) there is no cross-dialect name
    /// mapping, so a general function does not re-render across dialects with a different spelling.
    ///
    /// **Invariant:** the arguments are fully structural — no direct [`Literal`](ExprNode::Literal) and no
    /// [`Raw`](ExprNode::Raw). The reverse parser only produces this node from an *unquoted* call whose
    /// every argument lowered structurally: PostgreSQL synthesizes a `::type` cast on a literal argument
    /// (`f('x')` deparses as `f('x'::text)`) and stripping that cast to make the introspected form match
    /// could rewrite the DDL the canonical model feeds, so a literal-argument call stays `Raw`; and any
    /// unlowerable argument makes the *whole* call `Raw`. The KDL reader rejects a `function` node with a
    /// literal or `Raw` argument for the same reason, so a structural `Function` holds neither.
    Function { name: String, args: Vec<ExprNode> },
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
        collect_body_sources(&self.query, &mut sources);
        sources.into_iter()
    }
}

/// Collects every [`SourceRef`] reachable from a view body, recursing through set-op arms and subqueries.
fn collect_body_sources<'a>(body: &'a ViewBody, sources: &mut Vec<&'a SourceRef>) {
    match body {
        ViewBody::Select(query) => collect_query_sources(query, sources),
        ViewBody::Set {
            left,
            right,
            order_by,
            ..
        } => {
            collect_body_sources(left, sources);
            collect_body_sources(right, sources);
            for order in order_by {
                collect_expr_sources(&order.expr, sources);
            }
        }
        ViewBody::With {
            recursive,
            ctes,
            body,
        } => {
            // Drop only sources that are **in-scope CTE bindings** for the body being walked — the rest are
            // real view/table dependencies. Scope is per-body and declaration-ordered: a CTE body sees the
            // CTEs declared *before* it (plus *itself* when the `WITH` is `RECURSIVE`); the main body sees
            // all of them. A source matching a name NOT in that scope — e.g. a later-declared CTE's name in
            // a non-recursive `WITH`, or a self-name in a non-recursive CTE — is an existing sibling
            // relation and must be kept (dropping it would mis-order `ordered_views`). A **schema-qualified**
            // source (`public.dep`) is always a real relation (a CTE reference is unqualified), so it is
            // never dropped.
            for (index, cte) in ctes.iter().enumerate() {
                let mut in_scope: Vec<&str> =
                    ctes[..index].iter().map(|cte| cte.name.as_str()).collect();
                if *recursive {
                    in_scope.push(cte.name.as_str());
                }
                collect_body_sources_dropping_ctes(&cte.body, &in_scope, sources);
            }
            let all: Vec<&str> = ctes.iter().map(|cte| cte.name.as_str()).collect();
            collect_body_sources_dropping_ctes(body, &all, sources);
        }
    }
}

/// Collects the real (non-CTE) [`SourceRef`] dependencies of `body`, dropping any *unqualified* source
/// whose name is one of the in-scope CTE names `in_scope` (a local binding, not a dependency). Used by the
/// [`ViewBody::With`] scoping in [`collect_body_sources`].
fn collect_body_sources_dropping_ctes<'a>(
    body: &'a ViewBody,
    in_scope: &[&str],
    sources: &mut Vec<&'a SourceRef>,
) {
    let mut collected = Vec::new();
    collect_body_sources(body, &mut collected);
    sources.extend(
        collected
            .into_iter()
            .filter(|source| source.schema.is_some() || !in_scope.contains(&source.name.as_str())),
    );
}

/// Collects every [`SourceRef`] reachable from a single `SELECT` body, recursing through subqueries.
fn collect_query_sources<'a>(query: &'a ViewQueryModel, sources: &mut Vec<&'a SourceRef>) {
    // Introspected views carry no body but record their dependencies here; declared/package views
    // leave this empty and contribute their sources by walking the body below.
    sources.extend(query.dependencies.iter());
    if let Some(from) = &query.from {
        collect_source_item(from, sources);
    }
    for join in &query.joins {
        collect_source_item(&join.source, sources);
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

/// Collects the [`SourceRef`]s reachable from a `FROM`/`JOIN` source: a named relation is itself a
/// dependency; a derived table's alias is a local binding (not a dependency), so its body is walked
/// for the real relations it references.
fn collect_source_item<'a>(source: &'a SourceItem, sources: &mut Vec<&'a SourceRef>) {
    match source {
        SourceItem::Named(named) => sources.push(named),
        SourceItem::Derived { query, .. } => collect_body_sources(query, sources),
    }
}

/// Normalizes a constraint [`ExprNode`] to a canonical structural form, so expressions that
/// PostgreSQL's `pg_get_constraintdef` rewrites compare equal to the authored form once both the desired
/// and introspected model are normalized (applied in `canonicalize_model` before diffing). Two rewrites:
///
/// - **`BETWEEN` expansion**: `x BETWEEN a AND b` → `(x >= a) AND (x <= b)`, and
///   `x NOT BETWEEN a AND b` → `(x < a) OR (x > b)`.
/// - **Boolean re-nesting**: an associative `AND`/`OR` chain is flattened and re-folded
///   left-associatively, so `a AND (b AND c)` and `(a AND b) AND c` normalize to the same tree.
///
/// Recurses through the constraint-expression node set; leaves and view-body-only nodes (which never
/// occur in a check) are returned unchanged.
pub fn normalize_expr(expr: &ExprNode) -> ExprNode {
    match expr {
        ExprNode::Between {
            negated,
            operand,
            low,
            high,
        } => {
            let operand = normalize_expr(operand);
            let low = normalize_expr(low);
            let high = normalize_expr(high);
            let (op, lower_cmp, upper_cmp) = if *negated {
                (LogicalOp::Or, CompareOp::LessThan, CompareOp::GreaterThan)
            } else {
                (
                    LogicalOp::And,
                    CompareOp::GreaterThanOrEquals,
                    CompareOp::LessThanOrEquals,
                )
            };
            ExprNode::Logical {
                op,
                left: Box::new(ExprNode::Compare {
                    op: lower_cmp,
                    left: Box::new(operand.clone()),
                    right: Box::new(low),
                }),
                right: Box::new(ExprNode::Compare {
                    op: upper_cmp,
                    left: Box::new(operand),
                    right: Box::new(high),
                }),
            }
        }
        ExprNode::Logical { op, left, right } => {
            // Normalize each operand FIRST (so a nested `BETWEEN` is already expanded to its `AND`/`OR`
            // pair), then splice same-operator logicals into one flat chain and re-fold left-associatively.
            // This is why `y AND x BETWEEN 1 AND 2` normalizes to the same flat `y AND x >= 1 AND x <= 2`
            // tree PostgreSQL deparses, not `y AND (x >= 1 AND x <= 2)`.
            let mut terms = Vec::new();
            splice_same_op(*op, normalize_expr(left), &mut terms);
            splice_same_op(*op, normalize_expr(right), &mut terms);
            let mut terms = terms.into_iter();
            let mut acc = terms
                .next()
                .expect("a logical node always has at least two operands");
            for term in terms {
                acc = ExprNode::Logical {
                    op: *op,
                    left: Box::new(acc),
                    right: Box::new(term),
                };
            }
            acc
        }
        ExprNode::Binary { op, left, right } => ExprNode::Binary {
            op: *op,
            left: Box::new(normalize_expr(left)),
            right: Box::new(normalize_expr(right)),
        },
        ExprNode::Compare { op, left, right } => ExprNode::Compare {
            op: *op,
            left: Box::new(normalize_expr(left)),
            right: Box::new(normalize_expr(right)),
        },
        ExprNode::Not(inner) => ExprNode::Not(Box::new(normalize_expr(inner))),
        ExprNode::IsNull { negated, operand } => ExprNode::IsNull {
            negated: *negated,
            operand: Box::new(normalize_expr(operand)),
        },
        ExprNode::Like {
            case_insensitive,
            negated,
            operand,
            pattern,
        } => ExprNode::Like {
            case_insensitive: *case_insensitive,
            negated: *negated,
            operand: Box::new(normalize_expr(operand)),
            pattern: Box::new(normalize_expr(pattern)),
        },
        ExprNode::In {
            negated,
            operand,
            items,
        } => ExprNode::In {
            negated: *negated,
            operand: Box::new(normalize_expr(operand)),
            items: items.iter().map(normalize_expr).collect(),
        },
        ExprNode::ScalarFn { func, args } => ExprNode::ScalarFn {
            func: *func,
            args: args.iter().map(normalize_expr).collect(),
        },
        // A general function's name is folded to lowercase: the reverse parser only ever produces one
        // from an *unquoted* call (which PostgreSQL deparses lowercased), so lowercasing here — applied to
        // both the desired and introspected model in `canonicalize_model` — makes a model- or KDL-authored
        // `MD5(...)` compare equal to the introspected `md5(...)` instead of churning.
        ExprNode::Function { name, args } => ExprNode::Function {
            name: name.to_ascii_lowercase(),
            args: args.iter().map(normalize_expr).collect(),
        },
        other => other.clone(),
    }
}

/// Drops the case-insensitivity distinction on every [`ExprNode::Like`] node (forces
/// `case_insensitive = false`), for backends whose renderer emits the same `LIKE` for both flag states
/// and whose introspection therefore always reads `false` (MySQL/SQLite — only PostgreSQL spells the
/// case-insensitive form distinctly, as `ILIKE`). Applied to both the desired and introspected model in
/// `canonicalize_model` so an authored `ILIKE` (`case_insensitive: true`) check does not churn against
/// the introspected `false`. Recurses the constraint node set; other nodes pass through.
pub fn fold_like_case_insensitivity(expr: &ExprNode) -> ExprNode {
    match expr {
        ExprNode::Like {
            negated,
            operand,
            pattern,
            ..
        } => ExprNode::Like {
            case_insensitive: false,
            negated: *negated,
            operand: Box::new(fold_like_case_insensitivity(operand)),
            pattern: Box::new(fold_like_case_insensitivity(pattern)),
        },
        ExprNode::Binary { op, left, right } => ExprNode::Binary {
            op: *op,
            left: Box::new(fold_like_case_insensitivity(left)),
            right: Box::new(fold_like_case_insensitivity(right)),
        },
        ExprNode::Compare { op, left, right } => ExprNode::Compare {
            op: *op,
            left: Box::new(fold_like_case_insensitivity(left)),
            right: Box::new(fold_like_case_insensitivity(right)),
        },
        ExprNode::Logical { op, left, right } => ExprNode::Logical {
            op: *op,
            left: Box::new(fold_like_case_insensitivity(left)),
            right: Box::new(fold_like_case_insensitivity(right)),
        },
        ExprNode::Not(inner) => ExprNode::Not(Box::new(fold_like_case_insensitivity(inner))),
        ExprNode::IsNull { negated, operand } => ExprNode::IsNull {
            negated: *negated,
            operand: Box::new(fold_like_case_insensitivity(operand)),
        },
        ExprNode::In {
            negated,
            operand,
            items,
        } => ExprNode::In {
            negated: *negated,
            operand: Box::new(fold_like_case_insensitivity(operand)),
            items: items.iter().map(fold_like_case_insensitivity).collect(),
        },
        ExprNode::Between {
            negated,
            operand,
            low,
            high,
        } => ExprNode::Between {
            negated: *negated,
            operand: Box::new(fold_like_case_insensitivity(operand)),
            low: Box::new(fold_like_case_insensitivity(low)),
            high: Box::new(fold_like_case_insensitivity(high)),
        },
        ExprNode::ScalarFn { func, args } => ExprNode::ScalarFn {
            func: *func,
            args: args.iter().map(fold_like_case_insensitivity).collect(),
        },
        ExprNode::Function { name, args } => ExprNode::Function {
            name: name.clone(),
            args: args.iter().map(fold_like_case_insensitivity).collect(),
        },
        other => other.clone(),
    }
}

/// Splices an already-normalized operand into a same-operator `AND`/`OR` chain: if it is itself a
/// same-operator logical (a nested chain, or an expanded `BETWEEN`), its operands join the chain;
/// otherwise it is one term. Collapses a re-nested boolean tree to one canonical left-folded order.
fn splice_same_op(op: LogicalOp, node: ExprNode, terms: &mut Vec<ExprNode>) {
    match node {
        ExprNode::Logical {
            op: inner_op,
            left,
            right,
        } if inner_op == op => {
            splice_same_op(op, *left, terms);
            splice_same_op(op, *right, terms);
        }
        other => terms.push(other),
    }
}

/// Collects every [`SourceRef`] reachable from an expression, recursing through nested subqueries.
fn collect_expr_sources<'a>(expr: &'a ExprNode, sources: &mut Vec<&'a SourceRef>) {
    match expr {
        ExprNode::Column { .. }
        | ExprNode::BareColumn { .. }
        | ExprNode::Literal(_)
        | ExprNode::Raw(_) => {}
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
        ExprNode::ScalarFn { args, .. } | ExprNode::Function { args, .. } => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(column: &str) -> ExprNode {
        ExprNode::BareColumn {
            column: column.to_owned(),
        }
    }

    fn lit(text: &str) -> ExprNode {
        ExprNode::Literal(text.to_owned())
    }

    fn cmp(op: CompareOp, left: ExprNode, right: ExprNode) -> ExprNode {
        ExprNode::Compare {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn and(left: ExprNode, right: ExprNode) -> ExprNode {
        ExprNode::Logical {
            op: LogicalOp::And,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    #[test]
    fn normalize_expands_between() {
        // `x BETWEEN 0 AND 10` → `(x >= 0) AND (x <= 10)`, matching PostgreSQL's deparse.
        let between = ExprNode::Between {
            negated: false,
            operand: Box::new(bare("x")),
            low: Box::new(lit("0")),
            high: Box::new(lit("10")),
        };
        let expected = and(
            cmp(CompareOp::GreaterThanOrEquals, bare("x"), lit("0")),
            cmp(CompareOp::LessThanOrEquals, bare("x"), lit("10")),
        );
        assert_eq!(normalize_expr(&between), expected);
        // The introspected deparse form normalizes to the same tree (a no-op on it).
        assert_eq!(normalize_expr(&expected), expected);
    }

    #[test]
    fn normalize_expands_not_between() {
        let between = ExprNode::Between {
            negated: true,
            operand: Box::new(bare("x")),
            low: Box::new(lit("0")),
            high: Box::new(lit("10")),
        };
        let expected = ExprNode::Logical {
            op: LogicalOp::Or,
            left: Box::new(cmp(CompareOp::LessThan, bare("x"), lit("0"))),
            right: Box::new(cmp(CompareOp::GreaterThan, bare("x"), lit("10"))),
        };
        assert_eq!(normalize_expr(&between), expected);
    }

    #[test]
    fn normalize_flattens_between_inside_a_chain() {
        // `y AND (x BETWEEN 1 AND 2)` must normalize to the FLAT `y AND x >= 1 AND x <= 2` tree that
        // PostgreSQL deparses — the expanded BETWEEN joins the surrounding AND chain, not nested.
        let y = cmp(CompareOp::GreaterThan, bare("y"), lit("0"));
        let between = ExprNode::Between {
            negated: false,
            operand: Box::new(bare("x")),
            low: Box::new(lit("1")),
            high: Box::new(lit("2")),
        };
        let with_between = and(y.clone(), between);
        // The deparsed form: flat `(y AND x >= 1) AND x <= 2`.
        let deparsed = and(
            and(y, cmp(CompareOp::GreaterThanOrEquals, bare("x"), lit("1"))),
            cmp(CompareOp::LessThanOrEquals, bare("x"), lit("2")),
        );
        assert_eq!(normalize_expr(&with_between), deparsed);
        assert_eq!(normalize_expr(&with_between), normalize_expr(&deparsed));
    }

    #[test]
    fn normalize_reassociates_boolean_chains() {
        // Right-nested `a AND (b AND c)` and left-nested `(a AND b) AND c` normalize to the same tree.
        let a = cmp(CompareOp::GreaterThan, bare("a"), lit("0"));
        let bb = cmp(CompareOp::GreaterThan, bare("b"), lit("0"));
        let c = cmp(CompareOp::GreaterThan, bare("c"), lit("0"));
        let right_nested = and(a.clone(), and(bb.clone(), c.clone()));
        let left_nested = and(and(a.clone(), bb.clone()), c.clone());
        assert_eq!(normalize_expr(&right_nested), normalize_expr(&left_nested));
        assert_eq!(normalize_expr(&right_nested), left_nested);
    }

    #[test]
    fn normalize_lowercases_general_function_names() {
        // A model- or KDL-authored `MD5(col)` must compare equal to the introspected `md5(col)` (the
        // reverse parser only produces a `Function` node from an unquoted, hence lowercase, name), so
        // normalization folds the name and recurses into arguments.
        let upper = ExprNode::Function {
            name: "MD5".to_owned(),
            args: vec![bare("col")],
        };
        let lower = ExprNode::Function {
            name: "md5".to_owned(),
            args: vec![bare("col")],
        };
        assert_eq!(normalize_expr(&upper), lower);
        assert_eq!(normalize_expr(&lower), lower);
    }

    #[test]
    fn fold_like_ci_forces_case_sensitive() {
        // On MySQL/SQLite a `Like{case_insensitive: true}` renders as plain `LIKE` and reads back false;
        // folding both sides to false keeps an authored `ILIKE` check from churning.
        let insensitive = ExprNode::Like {
            case_insensitive: true,
            negated: false,
            operand: Box::new(bare("name")),
            pattern: Box::new(lit("'a%'")),
        };
        let folded = fold_like_case_insensitivity(&insensitive);
        assert_eq!(
            folded,
            ExprNode::Like {
                case_insensitive: false,
                negated: false,
                operand: Box::new(bare("name")),
                pattern: Box::new(lit("'a%'")),
            }
        );
        // Nested inside a boolean chain, the flag is folded too.
        let nested = and(
            insensitive,
            cmp(CompareOp::GreaterThan, bare("x"), lit("0")),
        );
        assert_eq!(
            fold_like_case_insensitivity(&nested),
            fold_like_case_insensitivity(&nested)
        );
        assert!(matches!(
            fold_like_case_insensitivity(&nested),
            ExprNode::Logical { .. }
        ));
    }

    #[test]
    fn with_scoping_keeps_out_of_scope_cte_named_sources() {
        // `WITH a AS (SELECT id FROM seed), seed AS (SELECT id FROM base) SELECT id FROM a`. In a
        // non-recursive `WITH`, `seed` is declared AFTER `a`, so the `seed` read inside `a` is a real
        // sibling relation (not the later CTE) and must survive as a dependency; the main body's `a`
        // reference is an in-scope CTE binding and is dropped. So `referenced_sources` = {seed, base}.
        let select_from = |name: &str| {
            ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "id".to_owned(),
                    expr: ExprNode::Column {
                        alias: "q".to_owned(),
                        column: "id".to_owned(),
                    },
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: name.to_owned(),
                    alias: "q".to_owned(),
                })),
                ..ViewQueryModel::default()
            }))
        };
        let view = ViewModel {
            name: "v".to_owned(),
            comment: None,
            columns: vec![ViewColumnModel {
                name: "id".to_owned(),
                ty: SqlType::I32,
                nullable: true,
            }],
            query: ViewBody::With {
                recursive: false,
                ctes: vec![
                    CteModel {
                        name: "a".to_owned(),
                        columns: Vec::new(),
                        body: select_from("seed"),
                    },
                    CteModel {
                        name: "seed".to_owned(),
                        columns: Vec::new(),
                        body: select_from("base"),
                    },
                ],
                body: Box::new(select_from("a")),
            },
        };
        let names: Vec<&str> = view.referenced_sources().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"seed"),
            "a later-declared CTE's name read by an earlier CTE is a real relation and must be kept: {names:?}",
        );
        assert!(
            names.contains(&"base"),
            "the `seed` CTE's real source: {names:?}"
        );
        assert!(
            !names.contains(&"a"),
            "the main body's reference to the in-scope CTE `a` must be dropped: {names:?}",
        );
    }
}
