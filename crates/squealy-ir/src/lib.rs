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
    /// SQL `%` modulo. Renders as `%` on every dialect, so a `%` expression round-trips structurally
    /// (introspect → re-render is byte-identical on the same backend). **Cross-dialect caveat:** the
    /// operand semantics are not identical — SQLite coerces both operands to integers before the
    /// operation (`9.5 % 2` → `1`), while PostgreSQL and MySQL keep the fractional remainder
    /// (`9.5 % 2` → `1.5`). Integer-operand modulo (the usual `col % n = 0` check) is portable; a
    /// non-integer operand is not. Unlike [`ArithmeticOp::Divide`] (whose neutral node is forced to a
    /// single fractional semantic per dialect), `Modulo` renders the bare operator as authored.
    Modulo,
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
    /// A MySQL `ON UPDATE CURRENT_TIMESTAMP` auto-update expression, structured like [`generated`]
    /// so the backend renders it in its own dialect and the diff compares it structurally.
    ///
    /// MySQL is the only dialect with this attribute, and the only expression it accepts is
    /// `CURRENT_TIMESTAMP` (so an introspected column carries [`ExprNode::Now`]); its fractional-seconds
    /// precision is forced to equal the column's own `TIMESTAMP`/`DATETIME` precision, so the renderer
    /// derives the fsp from the column type rather than from the node. PostgreSQL and SQLite reject a
    /// column carrying this — they cannot represent it.
    ///
    /// Boxed so the rare, MySQL-only attribute does not enlarge every `ColumnModel` (which is embedded
    /// inline in the plan/diff step enums), mirroring [`IndexModel::predicate`].
    ///
    /// [`generated`]: ColumnModel::generated
    pub on_update: Option<Box<ExprNode>>,
}

impl ColumnModel {
    /// Validates the [`on_update`](Self::on_update) attribute's shape, returning the reason it is
    /// unrepresentable or `None` when it is absent or well-formed.
    ///
    /// The neutral field exists solely to represent MySQL's `ON UPDATE CURRENT_TIMESTAMP`, so the only
    /// well-formed value is [`ExprNode::Now`] on a `TIMESTAMP`/`DATETIME` column that is not generated
    /// (an auto-update clause on a computed column is a contradiction, and MySQL rejects it). Both the
    /// capability preflight (for a backend that reports the attribute) and the MySQL renderer check this,
    /// so a malformed hand-authored package fails `check` rather than only at DDL-execution time.
    pub fn on_update_shape_error(&self) -> Option<&'static str> {
        let on_update = self.on_update.as_deref()?;
        if !matches!(on_update, ExprNode::Now) {
            return Some("an `ON UPDATE` attribute only supports `CURRENT_TIMESTAMP`");
        }
        if !matches!(self.ty, SqlType::Timestamp { .. }) {
            return Some(
                "an `ON UPDATE CURRENT_TIMESTAMP` attribute is only valid on a TIMESTAMP/DATETIME column",
            );
        }
        if self.generated.is_some() {
            return Some(
                "an `ON UPDATE CURRENT_TIMESTAMP` attribute is not allowed on a generated column",
            );
        }
        None
    }
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
    /// Backend-specific column prefix lengths by zero-based column position (MySQL `col(n)`, on a
    /// `UNIQUE`/`PRIMARY KEY` over a leading prefix of a string column). Sparse, like
    /// [`IndexModel::prefix_lengths`]; other backends reject a constraint carrying it.
    pub prefix_lengths: Vec<IndexPrefixLength>,
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
    /// Backend-specific column prefix lengths by zero-based key-term position (MySQL `col(n)`).
    pub prefix_lengths: Vec<IndexPrefixLength>,
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

/// Column prefix length for an indexed key term (MySQL indexes only a leading `length`-byte/character
/// prefix of the column, rendered as `col(length)`; other backends do not support it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexPrefixLength {
    pub position: usize,
    pub length: u32,
}

/// Validates a sparse list of column prefix lengths (an index's or a constraint's) against the key's
/// column count, returning a human-readable reason if malformed or `None` if valid. Each prefix must key
/// at least one character/byte (`col(0)` is invalid in MySQL) and name a real key-term position exactly
/// once. Shared by the capability preflight (so `check` fails fast) and each renderer (the plan path
/// skips the preflight) so both agree on what is renderable.
pub fn prefix_length_shape_error(
    num_columns: usize,
    prefix_lengths: &[IndexPrefixLength],
) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    for prefix in prefix_lengths {
        if prefix.length == 0 {
            return Some(format!(
                "has a zero-length prefix for key position {}",
                prefix.position
            ));
        }
        if prefix.position >= num_columns {
            return Some(format!(
                "has a prefix length for key position {} but only {num_columns} column(s)",
                prefix.position
            ));
        }
        if !seen.insert(prefix.position) {
            return Some(format!(
                "has duplicate prefix lengths for key position {}",
                prefix.position
            ));
        }
    }
    None
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

    /// Rewrites every result-pin type in the body by applying `f`, recursing through set-operation arms,
    /// `WITH` CTEs, derived-table subqueries, and scalar/`IN`/`EXISTS` subqueries.
    ///
    /// A result pin is the optional `CAST(<call> AS ty)` wrapper the renderer emits around an aggregate,
    /// window, `EXTRACT`, `CASE`, `NULLIF`, or `COALESCE` so the output column's wire type matches the
    /// view's declared column type (the `result: Option<SqlType>` fields). Because a dialect's cast
    /// vocabulary is many-to-one (several [`SqlType`]s render to the same keyword, so a narrower authored
    /// pin round-trips through introspection as the keyword's canonical representative), a backend's
    /// `canonical_view_body` calls this — on **both** the desired and the introspected model — to fold each
    /// pin to that same representative, so a published view does not churn. A general `CAST` node's target
    /// type is left untouched: it is a user-authored conversion, not a renderer-synthesized pin.
    pub fn map_result_pins(&mut self, f: &impl Fn(&SqlType) -> SqlType) {
        match self {
            ViewBody::Select(query) => map_query_result_pins(query, f),
            ViewBody::Set {
                left,
                right,
                order_by,
                ..
            } => {
                left.map_result_pins(f);
                right.map_result_pins(f);
                for order in order_by {
                    map_expr_result_pins(&mut order.expr, f);
                }
            }
            ViewBody::With { ctes, body, .. } => {
                for cte in ctes {
                    cte.body.map_result_pins(f);
                }
                body.map_result_pins(f);
            }
        }
    }

    /// Rewrites every [`SourceRef`] reachable from the body in place by applying `f`, recursing through
    /// set-operation arms, `WITH` CTEs, derived-table subqueries, and scalar/`IN`/`EXISTS` subqueries —
    /// the mutable analog of [`referenced_sources`](ViewModel::referenced_sources)'s traversal.
    ///
    /// A backend's `canonical_view_body` uses this — on **both** the desired and the introspected model —
    /// to reconcile a source-qualifier the backend does not round-trip. SQLite has no namespaces, so it
    /// suppresses the schema qualifier when rendering a view body: an introspected body's every
    /// `SourceRef.schema` reads back `None`, while a `from_database` desired body carries the mapped
    /// `Some("app")`. Flattening both sides to `None` lets a published view re-plan to empty (mirrors how
    /// [`canonical_schema_name`](crate::SchemaIntrospect::canonical_schema_name) flattens the top-level
    /// schema). Unlike [`referenced_sources`], this visits **every** `SourceRef` — including one bound to a
    /// `WITH` CTE (flattening its already-unqualified schema is a no-op) and the introspected
    /// `dependencies` — so a new source-bearing shape is handled uniformly.
    pub fn map_sources(&mut self, f: &impl Fn(&mut SourceRef)) {
        match self {
            ViewBody::Select(query) => map_query_sources(query, f),
            ViewBody::Set {
                left,
                right,
                order_by,
                ..
            } => {
                left.map_sources(f);
                right.map_sources(f);
                for order in order_by {
                    map_expr_sources(&mut order.expr, f);
                }
            }
            ViewBody::With { ctes, body, .. } => {
                for cte in ctes {
                    cte.body.map_sources(f);
                }
                body.map_sources(f);
            }
        }
    }

    /// Applies `f` to **every** [`ExprNode`] reachable from the body — projection, filter, join `ON`,
    /// `GROUP BY`, `HAVING`, `ORDER BY`, and set-op `ORDER BY` keys — recursing through derived-table
    /// subqueries, `WITH` CTEs, set-op arms, scalar/`IN`/`EXISTS` subqueries, and every nested
    /// sub-expression (each node, then its children). Exhaustive over [`ExprNode`], so a new variant is a
    /// compile error rather than a silently-unvisited node.
    ///
    /// A backend's `canonical_view_body` uses this — on **both** the desired and the introspected model —
    /// to fold a spelling its renderer does not distinguish. SQLite (like MySQL) emits plain `LIKE` for
    /// both `case_insensitive` states and introspects them back as `false`, so an authored `ILIKE`
    /// (`case_insensitive: true`) in a view body must be folded to `false` here or the reconstructed body
    /// churns a perpetual `CreateView` (mirrors `fold_like_case_insensitivity` on checks/index predicates).
    pub fn map_exprs(&mut self, f: &impl Fn(&mut ExprNode)) {
        match self {
            ViewBody::Select(query) => map_query_exprs(query, f),
            ViewBody::Set {
                left,
                right,
                order_by,
                ..
            } => {
                left.map_exprs(f);
                right.map_exprs(f);
                for order in order_by {
                    map_expr_nodes(&mut order.expr, f);
                }
            }
            ViewBody::With { ctes, body, .. } => {
                for cte in ctes {
                    cte.body.map_exprs(f);
                }
                body.map_exprs(f);
            }
        }
    }
}
/// Applies `f` to every [`ExprNode`] reachable from a single `SELECT` body (see [`ViewBody::map_exprs`]).
fn map_query_exprs(query: &mut ViewQueryModel, f: &impl Fn(&mut ExprNode)) {
    for item in &mut query.projection {
        map_expr_nodes(&mut item.expr, f);
    }
    if let Some(from) = &mut query.from {
        map_source_exprs(from, f);
    }
    for join in &mut query.joins {
        map_source_exprs(&mut join.source, f);
        if let Some(on) = &mut join.on {
            map_expr_nodes(on, f);
        }
    }
    if let Some(filter) = &mut query.filter {
        map_expr_nodes(filter, f);
    }
    for expr in &mut query.group_by {
        map_expr_nodes(expr, f);
    }
    if let Some(having) = &mut query.having {
        map_expr_nodes(having, f);
    }
    for order in &mut query.order_by {
        map_expr_nodes(&mut order.expr, f);
    }
}

/// A named source binds no expression; a derived table carries a whole sub-body to recurse into.
fn map_source_exprs(source: &mut SourceItem, f: &impl Fn(&mut ExprNode)) {
    match source {
        SourceItem::Named(_) => {}
        SourceItem::Derived { query, .. } => query.map_exprs(f),
    }
}

/// Applies `f` to `expr` itself and then to every nested [`ExprNode`], recursing into scalar/`IN`/`EXISTS`
/// subqueries. Exhaustive over [`ExprNode`] so a new node is a compile error here rather than an
/// unvisited one.
fn map_expr_nodes(expr: &mut ExprNode, f: &impl Fn(&mut ExprNode)) {
    f(expr);
    match expr {
        ExprNode::Column { .. }
        | ExprNode::BareColumn { .. }
        | ExprNode::Literal(_)
        | ExprNode::Raw(_)
        | ExprNode::Now => {}
        ExprNode::Binary { left, right, .. }
        | ExprNode::Compare { left, right, .. }
        | ExprNode::Logical { left, right, .. }
        | ExprNode::Nullif { left, right, .. } => {
            map_expr_nodes(left, f);
            map_expr_nodes(right, f);
        }
        ExprNode::Cast { operand, .. } | ExprNode::Aggregate { operand, .. } => {
            map_expr_nodes(operand, f);
        }
        ExprNode::Not(operand) | ExprNode::IsNull { operand, .. } => map_expr_nodes(operand, f),
        ExprNode::Like {
            operand, pattern, ..
        } => {
            map_expr_nodes(operand, f);
            map_expr_nodes(pattern, f);
        }
        ExprNode::In { operand, items, .. } => {
            map_expr_nodes(operand, f);
            for item in items {
                map_expr_nodes(item, f);
            }
        }
        ExprNode::Between {
            operand, low, high, ..
        } => {
            map_expr_nodes(operand, f);
            map_expr_nodes(low, f);
            map_expr_nodes(high, f);
        }
        ExprNode::ScalarSubquery(subquery) | ExprNode::Exists { subquery, .. } => {
            map_query_exprs(subquery, f);
        }
        ExprNode::InSubquery {
            operand, subquery, ..
        } => {
            map_expr_nodes(operand, f);
            map_query_exprs(subquery, f);
        }
        ExprNode::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                map_expr_nodes(arg, f);
            }
            for partition in partition_by {
                map_expr_nodes(partition, f);
            }
            for order in order_by {
                map_expr_nodes(&mut order.expr, f);
            }
        }
        ExprNode::Case { arms, else_, .. } => {
            for arm in arms {
                map_expr_nodes(&mut arm.when, f);
                map_expr_nodes(&mut arm.then, f);
            }
            if let Some(else_) = else_ {
                map_expr_nodes(else_, f);
            }
        }
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            ..
        } => {
            map_expr_nodes(operand, f);
            for arm in arms {
                map_expr_nodes(&mut arm.when, f);
                map_expr_nodes(&mut arm.then, f);
            }
            if let Some(else_) = else_ {
                map_expr_nodes(else_, f);
            }
        }
        ExprNode::Coalesce { args, .. }
        | ExprNode::ScalarFn { args, .. }
        | ExprNode::Function { args, .. } => {
            for arg in args {
                map_expr_nodes(arg, f);
            }
        }
        ExprNode::Extract { operand, .. }
        | ExprNode::DateTrunc { operand, .. }
        | ExprNode::ExtractSecond { operand, .. } => {
            map_expr_nodes(operand, f);
        }
    }
}

/// Applies `f` (read-only) to `expr` and every nested [`ExprNode`] in the **same scope**, stopping at a
/// nested subquery (a scalar/`IN`/`EXISTS` subquery is its own scope, whose columns bind to its own
/// sources — the [`ExprNode::InSubquery`] *operand* is same-scope and is visited). Mirrors the same-scope
/// traversal of the reverse parser's column resolver, so a caller inspecting a clause term's own-scope
/// column references (e.g. which projection aliases a `GROUP BY`/`HAVING`/`ORDER BY` names) sees exactly
/// those. Exhaustive over [`ExprNode`] so a new node is a compile error here rather than an unvisited one.
pub fn visit_scope_exprs(expr: &ExprNode, f: &mut impl FnMut(&ExprNode)) {
    f(expr);
    match expr {
        ExprNode::Column { .. }
        | ExprNode::BareColumn { .. }
        | ExprNode::Literal(_)
        | ExprNode::Raw(_)
        | ExprNode::Now => {}
        // A nested subquery is its own scope; its columns are irrelevant to the enclosing scope.
        ExprNode::ScalarSubquery(_) | ExprNode::Exists { .. } => {}
        ExprNode::InSubquery { operand, .. } => visit_scope_exprs(operand, f),
        ExprNode::Binary { left, right, .. }
        | ExprNode::Compare { left, right, .. }
        | ExprNode::Logical { left, right, .. }
        | ExprNode::Nullif { left, right, .. } => {
            visit_scope_exprs(left, f);
            visit_scope_exprs(right, f);
        }
        ExprNode::Cast { operand, .. } | ExprNode::Aggregate { operand, .. } => {
            visit_scope_exprs(operand, f)
        }
        ExprNode::Not(operand) | ExprNode::IsNull { operand, .. } => visit_scope_exprs(operand, f),
        ExprNode::Like {
            operand, pattern, ..
        } => {
            visit_scope_exprs(operand, f);
            visit_scope_exprs(pattern, f);
        }
        ExprNode::In { operand, items, .. } => {
            visit_scope_exprs(operand, f);
            for item in items {
                visit_scope_exprs(item, f);
            }
        }
        ExprNode::Between {
            operand, low, high, ..
        } => {
            visit_scope_exprs(operand, f);
            visit_scope_exprs(low, f);
            visit_scope_exprs(high, f);
        }
        ExprNode::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                visit_scope_exprs(arg, f);
            }
            for partition in partition_by {
                visit_scope_exprs(partition, f);
            }
            for order in order_by {
                visit_scope_exprs(&order.expr, f);
            }
        }
        ExprNode::Case { arms, else_, .. } => {
            for arm in arms {
                visit_scope_exprs(&arm.when, f);
                visit_scope_exprs(&arm.then, f);
            }
            if let Some(else_) = else_ {
                visit_scope_exprs(else_, f);
            }
        }
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            ..
        } => {
            visit_scope_exprs(operand, f);
            for arm in arms {
                visit_scope_exprs(&arm.when, f);
                visit_scope_exprs(&arm.then, f);
            }
            if let Some(else_) = else_ {
                visit_scope_exprs(else_, f);
            }
        }
        ExprNode::Coalesce { args, .. }
        | ExprNode::ScalarFn { args, .. }
        | ExprNode::Function { args, .. } => {
            for arg in args {
                visit_scope_exprs(arg, f);
            }
        }
        ExprNode::Extract { operand, .. }
        | ExprNode::DateTrunc { operand, .. }
        | ExprNode::ExtractSecond { operand, .. } => visit_scope_exprs(operand, f),
    }
}

/// Mutable twin of [`visit_scope_exprs`]: applies `f` to `expr` and every nested [`ExprNode`] in the **same
/// scope**, stopping at a nested subquery (its own scope). `f` may replace `*node` in place. Exhaustive over
/// [`ExprNode`] so a new node is a compile error here.
fn map_scope_exprs_mut(expr: &mut ExprNode, f: &mut impl FnMut(&mut ExprNode)) {
    f(expr);
    match expr {
        ExprNode::Column { .. }
        | ExprNode::BareColumn { .. }
        | ExprNode::Literal(_)
        | ExprNode::Raw(_)
        | ExprNode::Now => {}
        // A nested subquery is its own scope; its columns are irrelevant to the enclosing scope.
        ExprNode::ScalarSubquery(_) | ExprNode::Exists { .. } => {}
        ExprNode::InSubquery { operand, .. } => map_scope_exprs_mut(operand, f),
        ExprNode::Binary { left, right, .. }
        | ExprNode::Compare { left, right, .. }
        | ExprNode::Logical { left, right, .. }
        | ExprNode::Nullif { left, right, .. } => {
            map_scope_exprs_mut(left, f);
            map_scope_exprs_mut(right, f);
        }
        ExprNode::Cast { operand, .. } | ExprNode::Aggregate { operand, .. } => {
            map_scope_exprs_mut(operand, f)
        }
        ExprNode::Not(operand) | ExprNode::IsNull { operand, .. } => {
            map_scope_exprs_mut(operand, f)
        }
        ExprNode::Like {
            operand, pattern, ..
        } => {
            map_scope_exprs_mut(operand, f);
            map_scope_exprs_mut(pattern, f);
        }
        ExprNode::In { operand, items, .. } => {
            map_scope_exprs_mut(operand, f);
            for item in items {
                map_scope_exprs_mut(item, f);
            }
        }
        ExprNode::Between {
            operand, low, high, ..
        } => {
            map_scope_exprs_mut(operand, f);
            map_scope_exprs_mut(low, f);
            map_scope_exprs_mut(high, f);
        }
        ExprNode::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                map_scope_exprs_mut(arg, f);
            }
            for partition in partition_by {
                map_scope_exprs_mut(partition, f);
            }
            for order in order_by {
                map_scope_exprs_mut(&mut order.expr, f);
            }
        }
        ExprNode::Case { arms, else_, .. } => {
            for arm in arms {
                map_scope_exprs_mut(&mut arm.when, f);
                map_scope_exprs_mut(&mut arm.then, f);
            }
            if let Some(else_) = else_ {
                map_scope_exprs_mut(else_, f);
            }
        }
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            ..
        } => {
            map_scope_exprs_mut(operand, f);
            for arm in arms {
                map_scope_exprs_mut(&mut arm.when, f);
                map_scope_exprs_mut(&mut arm.then, f);
            }
            if let Some(else_) = else_ {
                map_scope_exprs_mut(else_, f);
            }
        }
        ExprNode::Coalesce { args, .. }
        | ExprNode::ScalarFn { args, .. }
        | ExprNode::Function { args, .. } => {
            for arg in args {
                map_scope_exprs_mut(arg, f);
            }
        }
        ExprNode::Extract { operand, .. }
        | ExprNode::DateTrunc { operand, .. }
        | ExprNode::ExtractSecond { operand, .. } => map_scope_exprs_mut(operand, f),
    }
}

/// Looks up the column names a named view-body source (a table or another view) exposes, so the clause-name
/// canonicalizer can tell a genuine source-column reference from a projection-alias reference. `None` when
/// the source is unknown to the catalog (e.g. a CTE-bound name or a source not in the introspected model) —
/// treated as "no colliding source column", so the reference resolves as the reverse parser classified it.
pub trait ViewSourceColumns {
    /// The column names `source` exposes, or `None` if the source is not in the catalog.
    fn source_columns(&self, source: &SourceRef) -> Option<Vec<String>>;
}

/// Converges a view body's clause terms (`GROUP BY`/`HAVING`/`ORDER BY`) to the structural form each backend
/// deparser produces, so a published view whose clause references a projection output **alias** re-plans to
/// empty instead of churning a `CREATE OR REPLACE VIEW` (git-bug 823ae69).
///
/// A backend that stores a view rewrites a clause's alias reference on introspection: a **standalone** alias
/// deparses to the projection's underlying **expression** (PostgreSQL `pg_get_viewdef`; MySQL `ORDER BY`),
/// while a **nested** reference (`ORDER BY total + 1`) deparses to the underlying **source column** (every
/// dialect resolves a nested reference to the source, not the output alias — verified live). The reverse
/// parser already applies each dialect's *position* rules when it decides whether a clause term is a kept
/// alias ([`ExprNode::BareColumn`]) or a bound source column ([`ExprNode::Column`]); what it cannot do is
/// distinguish a **nested** bare name that collides with a real source column from a projection alias, since
/// it has no source-column catalog. This pass, given that catalog, finishes the job:
///
/// - a clause reference to a projection alias is **expanded** to that projection's expression (the standalone
///   deparser form), and
/// - a clause name the (single) source relation actually exposes is **rebound** to `Column{source_alias,
///   name}` wherever that dialect resolves the name to the source column rather than the alias.
///
/// The one dialect subtlety, verified live: a name that collides with *both* a projection alias and a source
/// column resolves to the **alias** only as a standalone top-level `ORDER BY` term; a standalone `GROUP BY`
/// or `HAVING` name, and any *nested* reference, resolve to the **source column** (PostgreSQL, MySQL, and
/// SQLite agree). A name with no source collision is always the alias.
///
/// Then unreferenced [`ProjectionItem::internal_alias`]es are pruned. Applied to BOTH the desired and the
/// introspected model in `canonicalize_model`, so the two converge. The canonical model is the plan payload
/// (a `CreateView` for a changed view renders the canonicalized body), so every rewrite here must be
/// **semantically equivalent** to the original — an expanded alias (`ORDER BY total` → `ORDER BY (amount*2)`)
/// and a rebound source column are the same value the deparser would emit, matching the existing
/// `canonical_check_expression`/`normalize_expr` precedent (the canonical form is also rendered there). The
/// resolver is conservative — an ambiguous or unresolved source leaves a clause name unchanged rather than
/// risk a non-equivalent rewrite. Dialect-agnostic beyond a single `identifiers_case_insensitive` flag: the
/// parser encoded the dialect *position* rules already, so this needs only the catalog + the (structural)
/// standalone-vs-nested position and clause kind.
pub fn canonicalize_view_clause_aliases(
    body: &mut ViewBody,
    catalog: &dyn ViewSourceColumns,
    dialect: ViewClauseDialect,
    top_level_column_listed: bool,
) {
    // `top_level_column_listed` is `true` when the view declares an output column list (`view.columns` is
    // non-empty, which `render_create_view` emits as `CREATE VIEW (<columns>)`), suppressing each
    // projection's own `AS` — so only a kept `internal_alias` is an in-scope clause alias. When the view has
    // NO declared columns the renderer emits each projection `AS output_name`, so `output_name` IS visible.
    let ctx = ClauseCtx { catalog, dialect };
    canonicalize_view_body_scoped(
        body,
        &ctx,
        &std::collections::BTreeMap::new(),
        top_level_column_listed,
    );
}

/// How a backend resolves the case of a SQL identifier, for the view clause-alias canonicalizer's
/// collision/alias matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentifierCase {
    /// Case-sensitive (PostgreSQL, for quoted identifiers).
    Sensitive,
    /// Case-insensitive over ASCII only (SQLite).
    AsciiInsensitive,
    /// Case-insensitive with Unicode case folding (MySQL).
    UnicodeInsensitive,
}

/// The dialect properties the view clause-alias canonicalizer needs: how identifiers compare, whether a plain
/// `WITH` exposes a forward (later-sibling) CTE reference, and whether `WITH RECURSIVE` additionally does.
#[derive(Clone, Copy, Debug)]
pub struct ViewClauseDialect {
    pub identifier_case: IdentifierCase,
    /// A plain (non-recursive) `WITH` item may reference a LATER sibling — SQLite yes, PostgreSQL/MySQL no.
    pub cte_forward_references_visible: bool,
    /// `WITH RECURSIVE` additionally exposes later siblings — PostgreSQL yes, MySQL no (SQLite already yes via
    /// the field above).
    pub recursive_exposes_forward_ctes: bool,
}

/// The top-level output column names a view body projects (its declared output schema, as an enclosing scope
/// or catalog sees them) — used to resolve a view referenced as a source that declares no explicit columns.
pub fn view_body_output_names(body: &ViewBody) -> Vec<String> {
    derived_output_names(body)
}

/// A map from a `WITH`-bound CTE name to what it exposes for clause resolution, shadowing the global catalog.
/// `Some(columns)` is a CTE VISIBLE from the current scope; `None` marks a local CTE that exists in the
/// enclosing `WITH` but is NOT visible here (a forward reference the dialect disallows, or a non-recursive
/// self-reference) — such a name resolves to nothing rather than falling back to a same-named catalog table,
/// so a dialect-specific visibility difference can never bind a clause to the wrong relation and emit
/// different DDL. It only leaves the clause name un-canonicalized (a harmless re-plan).
type CteScope = std::collections::BTreeMap<String, Option<Vec<String>>>;

/// Inserts a CTE binding into `scope`, first removing any existing key that is the SAME identifier under this
/// backend's rules (a case-insensitive `foo` must shadow an outer `Foo`, not coexist with it — else a lookup
/// could return the wrong binding).
fn insert_cte(scope: &mut CteScope, name: &str, columns: Option<Vec<String>>, ctx: &ClauseCtx<'_>) {
    scope.retain(|key, _| !ctx.eq_ident(key, name));
    scope.insert(name.to_owned(), columns);
}

/// The invariant context threaded through the clause-alias canonicalization: the source-column catalog and
/// the backend's dialect properties.
struct ClauseCtx<'a> {
    catalog: &'a dyn ViewSourceColumns,
    dialect: ViewClauseDialect,
}

impl ClauseCtx<'_> {
    /// Compares two column/relation identifiers per this backend's case rules — exact (PostgreSQL), ASCII
    /// case-insensitive (SQLite), or Unicode case-insensitive (MySQL).
    fn eq_ident(&self, a: &str, b: &str) -> bool {
        match self.dialect.identifier_case {
            IdentifierCase::Sensitive => a == b,
            IdentifierCase::AsciiInsensitive => a.eq_ignore_ascii_case(b),
            // Unicode case folding: `to_lowercase` folds the full Unicode range (`Ä` == `ä`), as MySQL
            // resolves identifiers, so a non-ASCII clause name still collides with its source column.
            IdentifierCase::UnicodeInsensitive => {
                a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
            }
        }
    }
}

/// `outputs_column_listed` is `true` when an outer column list renames this scope's outputs (the top-level
/// view, a `WITH cte (columns)`, or the main body under the view's column list) — then a projection's own
/// `AS` is not in the `SELECT` scope, so only a kept `internal_alias` is a clause alias. It is `false` for a
/// scope whose projection aliases ARE in scope (a set arm, a column-list-less CTE, a derived table, a
/// subquery) — there a computed `output_name` is a referenceable alias. Mirrors the reverse parser's own
/// `outputs_column_listed` threading.
fn canonicalize_view_body_scoped(
    body: &mut ViewBody,
    ctx: &ClauseCtx<'_>,
    ctes: &CteScope,
    outputs_column_listed: bool,
) {
    match body {
        ViewBody::Select(query) => {
            canonicalize_query_clause_aliases(query, ctx, ctes, outputs_column_listed)
        }
        ViewBody::Set { left, right, .. } => {
            // A set's own trailing `ORDER BY` names the compound output columns by name (no source scope), so
            // it is left as-is; each arm names its own outputs by its projection aliases (a column list is
            // only a fallback), so an arm's `output_name` IS scope-visible.
            canonicalize_view_body_scoped(left, ctx, ctes, false);
            canonicalize_view_body_scoped(right, ctx, ctes, false);
        }
        ViewBody::With {
            recursive,
            ctes: cte_defs,
            body,
        } => {
            // A CTE body's VISIBLE siblings depend on the dialect: every dialect exposes a PRECEDING sibling;
            // SQLite also exposes a LATER sibling (a forward reference — `cte_forward_references_visible`),
            // while PostgreSQL/MySQL do not (and their `WITH RECURSIVE` forward-visibility differs, so it is
            // NOT inferred from `recursive`); a CTE body sees ITSELF only under `RECURSIVE`. Every LOCAL CTE
            // name is inserted into each body's scope either VISIBLE (`Some(columns)`) or HIDDEN (`None`) — a
            // hidden name resolves to nothing rather than a same-named catalog table, so a dialect-visibility
            // difference never binds a clause to the wrong relation (it only leaves the name bare, a harmless
            // re-plan). A CTE's own outputs are its declared column list, else its body's projection names.
            let recursive = *recursive;
            // Forward (later-sibling) visibility: a plain `WITH` exposes it on SQLite only; `WITH RECURSIVE`
            // additionally exposes it on PostgreSQL (but NOT MySQL). Both are per-backend properties, so this
            // is never inferred from `recursive` alone. (The hidden-vs-catalog distinction keeps a
            // mis-scoped source safe regardless — it resolves to nothing, never the wrong relation.)
            let forward = ctx.dialect.cte_forward_references_visible
                || (recursive && ctx.dialect.recursive_exposes_forward_ctes);
            let local: Vec<(String, Vec<String>)> = cte_defs
                .iter()
                .map(|cte| {
                    let columns = if cte.columns.is_empty() {
                        derived_output_names(&cte.body)
                    } else {
                        cte.columns.clone()
                    };
                    (cte.name.clone(), columns)
                })
                .collect();
            for (index, cte) in cte_defs.iter_mut().enumerate() {
                let mut body_scope = ctes.clone();
                for (other, (name, columns)) in local.iter().enumerate() {
                    let visible = other < index
                        || (forward && other > index)
                        || (other == index && recursive);
                    insert_cte(&mut body_scope, name, visible.then(|| columns.clone()), ctx);
                }
                // A CTE's outputs are column-listed exactly when it declares `WITH cte (columns)`.
                canonicalize_view_body_scoped(
                    &mut cte.body,
                    ctx,
                    &body_scope,
                    !cte.columns.is_empty(),
                );
            }
            // The main body sees every CTE; it inherits the enclosing view's column-list status.
            let mut main_scope = ctes.clone();
            for (name, columns) in &local {
                insert_cte(&mut main_scope, name, Some(columns.clone()), ctx);
            }
            canonicalize_view_body_scoped(body, ctx, &main_scope, outputs_column_listed);
        }
    }
}

/// Canonicalizes one `SELECT` scope's clause terms (see [`canonicalize_view_clause_aliases`]).
fn canonicalize_query_clause_aliases(
    query: &mut ViewQueryModel,
    ctx: &ClauseCtx<'_>,
    ctes: &CteScope,
    outputs_column_listed: bool,
) {
    // Recurse the nested scopes first: derived-table sources and any subqueries in this scope's expressions
    // (including its clause expressions) each resolve against their own sources.
    if let Some(from) = &mut query.from {
        canonicalize_source_clause_aliases(from, ctx, ctes);
    }
    for join in &mut query.joins {
        canonicalize_source_clause_aliases(&mut join.source, ctx, ctes);
    }
    let mut recurse_subqueries = |expr: &mut ExprNode| {
        if let Some(subquery) = expr_subquery_mut(expr) {
            // A subquery's own projection aliases are in scope (no outer column list).
            canonicalize_query_clause_aliases(subquery, ctx, ctes, false);
        }
    };
    for item in &mut query.projection {
        map_scope_exprs_mut(&mut item.expr, &mut recurse_subqueries);
    }
    if let Some(filter) = &mut query.filter {
        map_scope_exprs_mut(filter, &mut recurse_subqueries);
    }
    for join in &mut query.joins {
        if let Some(on) = &mut join.on {
            map_scope_exprs_mut(on, &mut recurse_subqueries);
        }
    }
    for expr in &mut query.group_by {
        map_scope_exprs_mut(expr, &mut recurse_subqueries);
    }
    if let Some(having) = &mut query.having {
        map_scope_exprs_mut(having, &mut recurse_subqueries);
    }
    for order in &mut query.order_by {
        map_scope_exprs_mut(&mut order.expr, &mut recurse_subqueries);
    }

    // Each of this scope's sources as `(alias, columns)`, in FROM-then-JOIN order (a `None` column set is a
    // source this pass cannot resolve — a missing/external relation).
    let mut sources: Vec<(String, Option<Vec<String>>)> = Vec::new();
    if let Some(from) = &query.from {
        sources.push((
            from.alias().to_owned(),
            source_item_columns(from, ctx, ctes),
        ));
    }
    for join in &query.joins {
        sources.push((
            join.source.alias().to_owned(),
            source_item_columns(&join.source, ctx, ctes),
        ));
    }
    let scope = ClauseScope {
        ctx,
        sources: &sources,
        outputs_column_listed,
    };

    let projection = query.projection.clone();
    for expr in &mut query.group_by {
        resolve_clause_term(expr, &projection, &scope, false);
    }
    if let Some(having) = &mut query.having {
        resolve_clause_term(having, &projection, &scope, false);
    }
    for order in &mut query.order_by {
        resolve_clause_term(&mut order.expr, &projection, &scope, true);
    }

    prune_unreferenced_clause_aliases(query, ctx);
}

/// What a clause-name resolver needs to know about the enclosing `SELECT` scope's sources.
struct ClauseScope<'a> {
    ctx: &'a ClauseCtx<'a>,
    /// Every source as `(alias, columns)` (`None` columns = unresolvable), used to detect a collision and to
    /// bind a name to the unique source that exposes it.
    sources: &'a [(String, Option<Vec<String>>)],
    outputs_column_listed: bool,
}

impl ClauseScope<'_> {
    /// Whether `name` matches a column of any *resolved* source (a possible collision), per this backend's
    /// identifier rules — a case-insensitive match on MySQL/SQLite, exact on PostgreSQL.
    fn collides(&self, name: &str) -> bool {
        self.sources
            .iter()
            .filter_map(|(_, columns)| columns.as_ref())
            .any(|columns| columns.iter().any(|c| self.ctx.eq_ident(c, name)))
    }

    /// The `(alias, column)` of the single resolved source that exposes `name`, or `None` if none or more than
    /// one does (an ambiguous join reference is left as the deparser qualified it). SQL binds a name a single
    /// source exposes to that source, and every dialect deparses it as a qualified column. The returned
    /// `column` is the source's OWN spelling (which may differ in case from the clause reference on a
    /// case-insensitive backend) — the form the deparser emits, so the rebound node matches introspection.
    fn unique_binding(&self, name: &str) -> Option<(&str, &str)> {
        let mut found = None;
        for (alias, columns) in self.sources {
            if let Some(column) = columns
                .as_ref()
                .and_then(|columns| columns.iter().find(|c| self.ctx.eq_ident(c, name)))
            {
                if found.is_some() {
                    return None;
                }
                found = Some((alias.as_str(), column.as_str()));
            }
        }
        found
    }

    /// Whether every source resolved — so a name that does not collide is definitely not a source column.
    fn all_sources_resolved(&self) -> bool {
        self.sources.iter().all(|(_, columns)| columns.is_some())
    }
}

/// The columns a `FROM`/`JOIN` source exposes: a derived table's own projection names, a `WITH`-bound CTE's
/// columns (shadowing the global catalog), else the global catalog's table/view.
fn source_item_columns(
    source: &SourceItem,
    ctx: &ClauseCtx<'_>,
    ctes: &CteScope,
) -> Option<Vec<String>> {
    match source {
        SourceItem::Derived { query, .. } => Some(derived_output_names(query)),
        SourceItem::Named(source) => {
            // A local CTE name (unqualified) shadows the global catalog, matched under this backend's
            // identifier rules (case-insensitive on MySQL/SQLite). A VISIBLE CTE yields its columns; a HIDDEN
            // local CTE (`None`) yields nothing — it is NOT resolved against a same-named catalog table, so a
            // dialect-visibility difference cannot bind a clause to the wrong relation. Only a name that is not
            // a local CTE at all falls through to the catalog.
            if source.schema.is_none()
                && let Some((_, columns)) = ctes
                    .iter()
                    .find(|(name, _)| ctx.eq_ident(name, &source.name))
            {
                return columns.clone();
            }
            ctx.catalog.source_columns(source)
        }
    }
}

/// Recurses into a derived-table source's own scope; a named source binds no columns of its own.
fn canonicalize_source_clause_aliases(
    source: &mut SourceItem,
    ctx: &ClauseCtx<'_>,
    ctes: &CteScope,
) {
    if let SourceItem::Derived { query, .. } = source {
        // A derived table's projection aliases are in scope (no outer column list).
        canonicalize_view_body_scoped(query, ctx, ctes, false);
    }
}

/// The mutable inner [`ViewQueryModel`] of a subquery node, or `None` for a non-subquery.
fn expr_subquery_mut(expr: &mut ExprNode) -> Option<&mut ViewQueryModel> {
    match expr {
        ExprNode::ScalarSubquery(query)
        | ExprNode::Exists {
            subquery: query, ..
        } => Some(query),
        ExprNode::InSubquery { subquery, .. } => Some(subquery),
        _ => None,
    }
}

/// The top-level output names a derived-table body projects (its columns, as an enclosing scope sees them).
fn derived_output_names(body: &ViewBody) -> Vec<String> {
    match body {
        ViewBody::Select(query) => query
            .projection
            .iter()
            .map(|item| item.output_name.clone())
            .collect(),
        ViewBody::Set { left, .. } => derived_output_names(left),
        ViewBody::With { body, .. } => derived_output_names(body),
    }
}

/// Resolves one clause term (see [`canonicalize_view_clause_aliases`]). `is_order_by` distinguishes an
/// `ORDER BY` term (whose *standalone* alias reference wins a source-column collision) from a `GROUP BY` /
/// `HAVING` term (whose collision resolves to the source column). A term that is *itself* a bare name is
/// standalone; a bare name nested in a larger expression is not.
fn resolve_clause_term(
    expr: &mut ExprNode,
    projection: &[ProjectionItem],
    scope: &ClauseScope<'_>,
    is_order_by: bool,
) {
    if let ExprNode::BareColumn { .. } = expr {
        resolve_clause_leaf(expr, projection, scope, true, is_order_by);
        return;
    }
    map_scope_exprs_mut(expr, &mut |node| {
        if let ExprNode::BareColumn { .. } = node {
            resolve_clause_leaf(node, projection, scope, false, is_order_by);
        }
    });
}

/// Expands a clause alias reference to its projection expression, or rebinds a source-column reference to its
/// source. A name colliding with a source column resolves to the **alias** only as a standalone `ORDER BY`
/// term; a standalone `GROUP BY`/`HAVING` name and any nested reference resolve to the **source column**. An
/// alias is expanded only after ruling out a hidden source collision across *all* sources, so an unknown or
/// join source leaves a possibly-source name bare rather than risk a false-equal.
fn resolve_clause_leaf(
    node: &mut ExprNode,
    projection: &[ProjectionItem],
    scope: &ClauseScope<'_>,
    standalone: bool,
    is_order_by: bool,
) {
    let ExprNode::BareColumn { column } = node else {
        return;
    };
    let name = column.clone();
    let alias_expr =
        referenceable_alias_expr(projection, &name, scope.outputs_column_listed, scope.ctx);

    // A standalone `ORDER BY` alias wins a source-column collision on every dialect (verified live), so it
    // expands regardless of the sources.
    if standalone && is_order_by {
        if let Some(expr) = alias_expr {
            expand_alias(node, expr, scope);
            return;
        }
        bind_source_column(node, scope, &name);
        return;
    }

    // Otherwise (a `GROUP BY`/`HAVING` term, or any nested reference) a name colliding with a source column is
    // that source column. It is safe to expand an alias only when NO source exposes the name AND every source
    // resolved — else a hidden collision (an unknown, or one of several join sources) could make two distinct
    // views compare equal.
    if scope.collides(&name) {
        bind_source_column(node, scope, &name);
        return;
    }
    if scope.all_sources_resolved()
        && let Some(expr) = alias_expr
    {
        expand_alias(node, expr, scope);
    }
    // A non-colliding name with an unresolved source is left bare (a harmless re-plan beats a false-equal); a
    // source column no single source uniquely exposes stays as the deparser qualified it.
}

/// Replaces a clause alias reference with the projection's expression, then binds any unqualified source
/// column the inserted expression carries (a hand-built/KDL projection may reference a source column bare) —
/// so the result is fully resolved and canonicalization is idempotent with the deparser's qualified form.
fn expand_alias(node: &mut ExprNode, expr: &ExprNode, scope: &ClauseScope<'_>) {
    *node = expr.clone();
    map_scope_exprs_mut(node, &mut |inserted| {
        if let ExprNode::BareColumn { column } = inserted {
            let name = column.clone();
            bind_source_column(inserted, scope, &name);
        }
    });
}

/// Rebinds a bare source-column reference to `Column{alias, name}` when exactly one resolved source exposes
/// the name; an ambiguous (or unresolved) reference is left as the deparser qualified it.
fn bind_source_column(node: &mut ExprNode, scope: &ClauseScope<'_>, name: &str) {
    if let Some((alias, column)) = scope.unique_binding(name) {
        *node = ExprNode::Column {
            alias: alias.to_owned(),
            column: column.to_owned(),
        };
    }
}

/// The expression of the projection a clause name references as an output alias, or `None` if none does. A
/// kept [`ProjectionItem::internal_alias`] (the explicit `AS` recorded because a clause references it) is
/// always an in-scope alias. A projection's own `output_name` is an in-scope alias only when this scope's
/// outputs are NOT renamed by an outer column list (`!outputs_column_listed`) — a set arm, a column-list-less
/// CTE, a derived table, or a subquery — and only for a *computed* or *renamed* projection (a plain column
/// named after itself, `q.id AS id`, is the source column, not an alias). Mirrors the reverse parser's
/// `computed_aliases`. Name matching follows the backend's identifier rules (case-insensitive on
/// MySQL/SQLite), the same as source-column collision detection.
fn referenceable_alias_expr<'a>(
    projection: &'a [ProjectionItem],
    name: &str,
    outputs_column_listed: bool,
    ctx: &ClauseCtx<'_>,
) -> Option<&'a ExprNode> {
    if let Some(item) = projection.iter().find(|item| {
        item.internal_alias
            .as_deref()
            .is_some_and(|a| ctx.eq_ident(a, name))
    }) {
        return Some(&item.expr);
    }
    if outputs_column_listed {
        return None;
    }
    projection.iter().find_map(|item| {
        let self_named = matches!(
            &item.expr,
            ExprNode::Column { column, .. } | ExprNode::BareColumn { column } if ctx.eq_ident(column, name)
        );
        (item.internal_alias.is_none() && ctx.eq_ident(&item.output_name, name) && !self_named)
            .then_some(&item.expr)
    })
}

/// Clears each [`ProjectionItem::internal_alias`] no remaining clause term references (expansion above
/// removed the references that were expanded); a backend that stores the view drops such an alias, so keeping
/// it would churn. Mirrors the reverse parser's own pruning.
fn prune_unreferenced_clause_aliases(query: &mut ViewQueryModel, ctx: &ClauseCtx<'_>) {
    if query
        .projection
        .iter()
        .all(|item| item.internal_alias.is_none())
    {
        return;
    }
    let mut referenced: Vec<String> = Vec::new();
    // An opaque `Raw` clause (a legacy-package or hand-built body) may reference an alias by name that this
    // structural scan cannot see, so treat any `Raw` in a clause as referencing every alias — retain them all
    // rather than emit a `CREATE VIEW` whose clause dangles.
    let mut has_raw = false;
    let mut note = |expr: &ExprNode| match expr {
        ExprNode::BareColumn { column } => referenced.push(column.clone()),
        ExprNode::Raw(_) => has_raw = true,
        _ => {}
    };
    for expr in query.group_by.iter().chain(query.having.iter()) {
        visit_scope_exprs(expr, &mut note);
    }
    for order in &query.order_by {
        visit_scope_exprs(&order.expr, &mut note);
    }
    if has_raw {
        return;
    }
    for item in &mut query.projection {
        // An alias is referenced when a remaining clause bare name matches it under this backend's identifier
        // rules (case-insensitive on MySQL/SQLite), the same comparison alias lookup uses.
        if let Some(alias) = &item.internal_alias
            && !referenced.iter().any(|name| ctx.eq_ident(name, alias))
        {
            item.internal_alias = None;
        }
    }
}

/// Applies `f` to every result pin reachable from a single `SELECT` body (see
/// [`ViewBody::map_result_pins`]).
fn map_query_result_pins(query: &mut ViewQueryModel, f: &impl Fn(&SqlType) -> SqlType) {
    for item in &mut query.projection {
        map_expr_result_pins(&mut item.expr, f);
    }
    if let Some(from) = &mut query.from {
        map_source_result_pins(from, f);
    }
    for join in &mut query.joins {
        map_source_result_pins(&mut join.source, f);
        if let Some(on) = &mut join.on {
            map_expr_result_pins(on, f);
        }
    }
    if let Some(filter) = &mut query.filter {
        map_expr_result_pins(filter, f);
    }
    for expr in &mut query.group_by {
        map_expr_result_pins(expr, f);
    }
    if let Some(having) = &mut query.having {
        map_expr_result_pins(having, f);
    }
    for order in &mut query.order_by {
        map_expr_result_pins(&mut order.expr, f);
    }
}

/// A named source binds no expression; a derived table carries a whole sub-body to recurse into.
fn map_source_result_pins(source: &mut SourceItem, f: &impl Fn(&SqlType) -> SqlType) {
    match source {
        SourceItem::Named(_) => {}
        SourceItem::Derived { query, .. } => query.map_result_pins(f),
    }
}

/// Applies `f` to every result pin reachable from an expression (see [`ViewBody::map_result_pins`]).
/// Exhaustive over [`ExprNode`] so a new pin-carrying variant is a compile error here rather than a
/// silently-unfolded pin.
fn map_expr_result_pins(expr: &mut ExprNode, f: &impl Fn(&SqlType) -> SqlType) {
    match expr {
        ExprNode::Aggregate {
            operand, result, ..
        } => {
            map_expr_result_pins(operand, f);
            map_pin(result, f);
        }
        ExprNode::Window {
            args,
            partition_by,
            order_by,
            result,
            ..
        } => {
            for arg in args {
                map_expr_result_pins(arg, f);
            }
            for part in partition_by {
                map_expr_result_pins(part, f);
            }
            for order in order_by {
                map_expr_result_pins(&mut order.expr, f);
            }
            map_pin(result, f);
        }
        ExprNode::Case {
            arms,
            else_,
            result,
        } => {
            map_case_arms(arms, else_, f);
            map_pin(result, f);
        }
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            result,
        } => {
            map_expr_result_pins(operand, f);
            map_case_arms(arms, else_, f);
            map_pin(result, f);
        }
        ExprNode::Nullif {
            left,
            right,
            result,
        } => {
            map_expr_result_pins(left, f);
            map_expr_result_pins(right, f);
            map_pin(result, f);
        }
        ExprNode::Coalesce { args, result } => {
            for arg in args {
                map_expr_result_pins(arg, f);
            }
            map_pin(result, f);
        }
        ExprNode::Extract {
            operand, result, ..
        }
        | ExprNode::ExtractSecond { operand, result } => {
            map_expr_result_pins(operand, f);
            map_pin(result, f);
        }
        ExprNode::Binary { left, right, .. }
        | ExprNode::Compare { left, right, .. }
        | ExprNode::Logical { left, right, .. } => {
            map_expr_result_pins(left, f);
            map_expr_result_pins(right, f);
        }
        // A general `CAST`'s target type is a user conversion, not a renderer pin — leave it.
        ExprNode::Cast { operand, .. } => map_expr_result_pins(operand, f),
        ExprNode::Not(operand) | ExprNode::IsNull { operand, .. } => {
            map_expr_result_pins(operand, f)
        }
        ExprNode::Like {
            operand, pattern, ..
        } => {
            map_expr_result_pins(operand, f);
            map_expr_result_pins(pattern, f);
        }
        ExprNode::In { operand, items, .. } => {
            map_expr_result_pins(operand, f);
            for item in items {
                map_expr_result_pins(item, f);
            }
        }
        ExprNode::Between {
            operand, low, high, ..
        } => {
            map_expr_result_pins(operand, f);
            map_expr_result_pins(low, f);
            map_expr_result_pins(high, f);
        }
        ExprNode::ScalarSubquery(query) => map_query_result_pins(query, f),
        ExprNode::InSubquery {
            operand, subquery, ..
        } => {
            map_expr_result_pins(operand, f);
            map_query_result_pins(subquery, f);
        }
        ExprNode::Exists { subquery, .. } => map_query_result_pins(subquery, f),
        ExprNode::ScalarFn { args, .. } | ExprNode::Function { args, .. } => {
            for arg in args {
                map_expr_result_pins(arg, f);
            }
        }
        ExprNode::DateTrunc { operand, .. } => map_expr_result_pins(operand, f),
        // Leaves — no nested expression, no result pin.
        ExprNode::Column { .. }
        | ExprNode::BareColumn { .. }
        | ExprNode::Literal(_)
        | ExprNode::Raw(_)
        | ExprNode::Now => {}
    }
}

/// Folds a result pin in place through `f` when present.
fn map_pin(result: &mut Option<SqlType>, f: &impl Fn(&SqlType) -> SqlType) {
    if let Some(ty) = result {
        *ty = f(ty);
    }
}

/// Applies `f` to every `WHEN`/`THEN` arm and the optional `ELSE` of a `CASE` body.
fn map_case_arms(
    arms: &mut [CaseArm],
    else_: &mut Option<Box<ExprNode>>,
    f: &impl Fn(&SqlType) -> SqlType,
) {
    for arm in arms {
        map_expr_result_pins(&mut arm.when, f);
        map_expr_result_pins(&mut arm.then, f);
    }
    if let Some(else_) = else_ {
        map_expr_result_pins(else_, f);
    }
}

/// Applies `f` to every [`SourceRef`] reachable from a single `SELECT` body, recursing through subqueries
/// (see [`ViewBody::map_sources`]). Mirrors [`collect_query_sources`], including the introspected
/// `dependencies`.
fn map_query_sources(query: &mut ViewQueryModel, f: &impl Fn(&mut SourceRef)) {
    for dependency in &mut query.dependencies {
        f(dependency);
    }
    if let Some(from) = &mut query.from {
        map_source_item_sources(from, f);
    }
    for join in &mut query.joins {
        map_source_item_sources(&mut join.source, f);
        if let Some(on) = &mut join.on {
            map_expr_sources(on, f);
        }
    }
    for item in &mut query.projection {
        map_expr_sources(&mut item.expr, f);
    }
    if let Some(filter) = &mut query.filter {
        map_expr_sources(filter, f);
    }
    for expr in &mut query.group_by {
        map_expr_sources(expr, f);
    }
    if let Some(having) = &mut query.having {
        map_expr_sources(having, f);
    }
    for order in &mut query.order_by {
        map_expr_sources(&mut order.expr, f);
    }
}

/// Applies `f` to the [`SourceRef`]s of a `FROM`/`JOIN` source: a named relation is one itself; a derived
/// table's body is walked (its alias is a local binding, not a source).
fn map_source_item_sources(source: &mut SourceItem, f: &impl Fn(&mut SourceRef)) {
    match source {
        SourceItem::Named(named) => f(named),
        SourceItem::Derived { query, .. } => query.map_sources(f),
    }
}

/// Applies `f` to every [`SourceRef`] reachable from an expression, recursing into scalar/`IN`/`EXISTS`
/// subqueries (see [`ViewBody::map_sources`]). Exhaustive over [`ExprNode`], mirroring
/// [`collect_expr_sources`], so a new source-bearing variant is a compile error here rather than a
/// silently-unvisited source.
fn map_expr_sources(expr: &mut ExprNode, f: &impl Fn(&mut SourceRef)) {
    match expr {
        ExprNode::Column { .. }
        | ExprNode::BareColumn { .. }
        | ExprNode::Literal(_)
        | ExprNode::Raw(_)
        | ExprNode::Now => {}
        ExprNode::Binary { left, right, .. }
        | ExprNode::Compare { left, right, .. }
        | ExprNode::Logical { left, right, .. }
        | ExprNode::Nullif { left, right, .. } => {
            map_expr_sources(left, f);
            map_expr_sources(right, f);
        }
        ExprNode::Cast { operand, .. } | ExprNode::Aggregate { operand, .. } => {
            map_expr_sources(operand, f);
        }
        ExprNode::Not(operand) | ExprNode::IsNull { operand, .. } => map_expr_sources(operand, f),
        ExprNode::Like {
            operand, pattern, ..
        } => {
            map_expr_sources(operand, f);
            map_expr_sources(pattern, f);
        }
        ExprNode::In { operand, items, .. } => {
            map_expr_sources(operand, f);
            for item in items {
                map_expr_sources(item, f);
            }
        }
        ExprNode::Between {
            operand, low, high, ..
        } => {
            map_expr_sources(operand, f);
            map_expr_sources(low, f);
            map_expr_sources(high, f);
        }
        ExprNode::ScalarSubquery(subquery) | ExprNode::Exists { subquery, .. } => {
            map_query_sources(subquery, f);
        }
        ExprNode::InSubquery {
            operand, subquery, ..
        } => {
            map_expr_sources(operand, f);
            map_query_sources(subquery, f);
        }
        ExprNode::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                map_expr_sources(arg, f);
            }
            for partition in partition_by {
                map_expr_sources(partition, f);
            }
            for order in order_by {
                map_expr_sources(&mut order.expr, f);
            }
        }
        ExprNode::Case { arms, else_, .. } => {
            for arm in arms {
                map_expr_sources(&mut arm.when, f);
                map_expr_sources(&mut arm.then, f);
            }
            if let Some(else_) = else_ {
                map_expr_sources(else_, f);
            }
        }
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            ..
        } => {
            map_expr_sources(operand, f);
            for arm in arms {
                map_expr_sources(&mut arm.when, f);
                map_expr_sources(&mut arm.then, f);
            }
            if let Some(else_) = else_ {
                map_expr_sources(else_, f);
            }
        }
        ExprNode::Coalesce { args, .. }
        | ExprNode::ScalarFn { args, .. }
        | ExprNode::Function { args, .. } => {
            for arg in args {
                map_expr_sources(arg, f);
            }
        }
        ExprNode::Extract { operand, .. }
        | ExprNode::DateTrunc { operand, .. }
        | ExprNode::ExtractSecond { operand, .. } => {
            map_expr_sources(operand, f);
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
    /// The projection's own explicit `AS <alias>` — the name the body's own `ORDER BY`/`GROUP BY`/`HAVING`
    /// reference — kept when a `CREATE VIEW (<cols>)`/`WITH cte (<cols>)` list names the outputs (so it
    /// suppresses the `AS`) **and** a body clause actually references it. `None` otherwise: no column list, a
    /// column-listed projection with no explicit `AS` (a bare column's own name is not a suppressible `AS`),
    /// or an alias no clause references (the reverse parser prunes it, since the renderer would emit a
    /// needless `AS` that a backend drops on storage). Kept even when the alias *coincides* with
    /// [`output_name`](Self::output_name) — a column list does not introduce its names into the `SELECT`
    /// scope, so a bare clause reference still needs the explicit `AS`. When `Some`, the renderer re-emits
    /// `AS <internal_alias>` even under a column list so the body-local clause reference resolves (without it
    /// the reference dangles — invalid SQL). Re-rendering a column-listed body is byte-identical to its
    /// source. The typed view builder never produces this (its clauses reference only source columns); it
    /// arises from external/hand-authored SQL and KDL packages.
    ///
    /// **Residual (a harmless re-plan, never wrong SQL):** the reverse parser has no source-column catalog,
    /// so it cannot always tell a clause's alias reference from a same-named source column, and a backend
    /// that stores a view rewrites clause references on introspection. Two shapes therefore re-plan a
    /// `CREATE OR REPLACE VIEW` each run — the same idempotent, non-destructive convergence gap the
    /// body-unknown view path already accepts. First, a backend rewrites a clause's alias reference to the
    /// underlying expression (PostgreSQL `pg_get_viewdef` deparses `… AS total … ORDER BY total` as
    /// `… AS n … ORDER BY (<expr>)`; MySQL expands an expression-alias clause), so the introspected body no
    /// longer carries the alias. Second, a source column whose name collides with a computed projection
    /// alias — a bare clause reference is kept as an alias here, but a dialect resolves it to the source
    /// column. SQLite (verbatim DDL) round-trips to empty except under such a collision. The re-render is
    /// always valid SQL and preserves the view's meaning; only the diff sees a difference. Removing the
    /// residual needs catalog-based name resolution (tracked separately).
    pub internal_alias: Option<String>,
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
        ViewBody::With { ctes, body, .. } => {
            // Every CTE name in a `WITH` is a local binding for every body in it — including a *later*-
            // declared CTE (forward reference) and a CTE's own body — and it **shadows** any same-named
            // external relation. (Verified on SQLite: a forward CTE reference resolves to the CTE and hides
            // an external table of that name; even a non-recursive self-reference is a "circular reference",
            // i.e. the name is in scope. PostgreSQL/MySQL reject a forward/self reference outright, so a view
            // carrying that shape is SQLite-only, where mutual visibility holds.) So an unqualified source
            // whose name matches ANY CTE is a local binding, not a view/table dependency, and is dropped —
            // dropping it is what keeps `ordered_views` from creating this view after (or in a false cycle
            // with) an unrelated sibling that merely shares a CTE's name. A **schema-qualified** source
            // (`public.dep`) is always a real relation (a CTE reference is unqualified), so it is kept.
            let mut inner = Vec::new();
            for cte in ctes {
                collect_body_sources(&cte.body, &mut inner);
            }
            collect_body_sources(body, &mut inner);
            let bound: Vec<&str> = ctes.iter().map(|cte| cte.name.as_str()).collect();
            sources.extend(inner.into_iter().filter(|source| {
                source.schema.is_some() || !bound.contains(&source.name.as_str())
            }));
        }
    }
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
/// Applies `f` to the target type of every general [`ExprNode::Cast`] reachable from `expr` (recursing
/// through all sub-expressions). A backend's constraint canonicalization uses this — on **both** the
/// desired and introspected model — to fold each general cast's target to that dialect's canonical
/// representative (a cast vocabulary is many-to-one, so several [`SqlType`]s render to the same keyword).
///
/// A cast structured from a `Raw` string on both sides already agrees (both re-parse through the same
/// inverter); this covers the STRUCTURAL-desired-cast path that never re-parses — a narrower cast in a
/// KDL package deployed to a lossier dialect, or a hand-built model — which would otherwise churn against
/// the introspected representative. This is the general-cast analogue of result-pin folding
/// ([`ViewBody::map_result_pins`]), which deliberately leaves a general cast's target alone.
pub fn map_cast_types(expr: &mut ExprNode, f: &impl Fn(&SqlType) -> SqlType) {
    map_expr_nodes(expr, &|node| {
        if let ExprNode::Cast { ty, .. } = node {
            *ty = f(ty);
        }
    });
}

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
        // Recurse through a general cast's operand — a `CAST(<expr> AS ty)` check carries a full
        // expression that itself needs normalizing (e.g. a `BETWEEN` inside the cast must expand to its
        // `AND` pair to match PostgreSQL's deparse), else `CAST((x BETWEEN 1 AND 2) AS boolean)` would
        // churn against the introspected `CAST((x >= 1 AND x <= 2) AS boolean)`.
        ExprNode::Cast { operand, ty } => ExprNode::Cast {
            operand: Box::new(normalize_expr(operand)),
            ty: ty.clone(),
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
        // A general cast carries a full expression that may contain a `Like` — recurse so a
        // `CAST(x ILIKE 'a%' AS ty)` desired check folds to `case_insensitive: false` on MySQL/SQLite
        // (which render both flag states as plain `LIKE`) instead of churning.
        ExprNode::Cast { operand, ty } => ExprNode::Cast {
            operand: Box::new(fold_like_case_insensitivity(operand)),
            ty: ty.clone(),
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

    fn on_update_column(ty: SqlType, on_update: Option<ExprNode>) -> ColumnModel {
        ColumnModel {
            name: "updated_at".to_owned(),
            comment: None,
            ty,
            collation: None,
            nullable: false,
            default: None,
            identity: None,
            generated: None,
            on_update: on_update.map(Box::new),
        }
    }

    fn timestamp() -> SqlType {
        SqlType::Timestamp {
            tz: true,
            precision: None,
        }
    }

    #[test]
    fn on_update_shape_error_accepts_now_on_a_temporal_column() {
        assert!(
            on_update_column(timestamp(), None)
                .on_update_shape_error()
                .is_none()
        );
        assert!(
            on_update_column(timestamp(), Some(ExprNode::Now))
                .on_update_shape_error()
                .is_none()
        );
    }

    #[test]
    fn on_update_shape_error_rejects_a_non_now_node() {
        assert!(
            on_update_column(timestamp(), Some(ExprNode::Raw("now() + 1".to_owned())))
                .on_update_shape_error()
                .is_some()
        );
    }

    #[test]
    fn on_update_shape_error_rejects_a_non_temporal_column() {
        assert!(
            on_update_column(SqlType::I32, Some(ExprNode::Now))
                .on_update_shape_error()
                .is_some()
        );
    }

    #[test]
    fn on_update_shape_error_rejects_a_generated_column() {
        let mut column = on_update_column(timestamp(), Some(ExprNode::Now));
        column.generated = Some(GeneratedColumnModel {
            expression: Some(ExprNode::Now),
            storage: GeneratedStorage::Virtual,
        });
        assert!(column.on_update_shape_error().is_some());
    }

    fn bare(column: &str) -> ExprNode {
        ExprNode::BareColumn {
            column: column.to_owned(),
        }
    }

    fn lit(text: &str) -> ExprNode {
        ExprNode::Literal(text.to_owned())
    }

    // ---- clause-alias canonicalization (git-bug 823ae69) ------------------------------------------

    /// A catalog exposing a fixed column set for the single source alias `q`, and nothing else.
    struct QCatalog(Vec<String>);
    impl ViewSourceColumns for QCatalog {
        fn source_columns(&self, source: &SourceRef) -> Option<Vec<String>> {
            (source.name == "t").then(|| self.0.clone())
        }
    }

    fn q_source() -> SourceItem {
        SourceItem::Named(SourceRef {
            schema: None,
            name: "t".to_owned(),
            alias: "q".to_owned(),
        })
    }

    fn q_col(column: &str) -> ExprNode {
        ExprNode::Column {
            alias: "q".to_owned(),
            column: column.to_owned(),
        }
    }

    /// `amount * 2` over source `q`.
    fn amount_times_two() -> ExprNode {
        ExprNode::Binary {
            op: ArithmeticOp::Multiply,
            left: Box::new(q_col("amount")),
            right: Box::new(lit("2")),
        }
    }

    /// A single-source `SELECT (amount*2) AS total FROM t q` carrying the given clause `order_by`.
    fn total_view(internal_alias: Option<&str>, order_by: Vec<ExprNode>) -> ViewQueryModel {
        ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: internal_alias.map(str::to_owned),
                expr: amount_times_two(),
            }],
            from: Some(q_source()),
            order_by: order_by
                .into_iter()
                .map(|expr| OrderItem {
                    expr,
                    direction: None,
                    nulls: None,
                })
                .collect(),
            ..ViewQueryModel::default()
        }
    }

    /// A test dialect: `identifier_case` per the argument (fixtures use lower-case names so the choice is
    /// usually inert), with the given plain-`WITH` forward visibility and no recursive-forward exposure.
    fn test_dialect(identifier_case: IdentifierCase, forward: bool) -> ViewClauseDialect {
        ViewClauseDialect {
            identifier_case,
            cte_forward_references_visible: forward,
            recursive_exposes_forward_ctes: false,
        }
    }

    fn canon(query: ViewQueryModel, catalog: &dyn ViewSourceColumns) -> ViewQueryModel {
        let mut body = ViewBody::Select(Box::new(query));
        // Case-sensitive identifiers (PostgreSQL); the fixtures use lower-case names, so the choice is inert.
        canonicalize_view_clause_aliases(
            &mut body,
            catalog,
            test_dialect(IdentifierCase::Sensitive, false),
            true,
        );
        match body {
            ViewBody::Select(query) => *query,
            _ => unreachable!(),
        }
    }

    #[test]
    fn standalone_order_by_alias_expands_to_the_projection_expression() {
        // `... AS total ... ORDER BY total` — no source column `total`. The alias expands to `amount*2`,
        // and the now-unreferenced internal alias is pruned. (Matches PG/MySQL `pg_get_viewdef`/deparse.)
        let out = canon(
            total_view(Some("total"), vec![bare("total")]),
            &QCatalog(vec!["amount".to_owned()]),
        );
        assert_eq!(out.order_by[0].expr, amount_times_two());
        assert_eq!(out.projection[0].internal_alias, None);
    }

    #[test]
    fn standalone_order_by_alias_expands_even_when_a_source_column_collides() {
        // Source `t` also has a `total` column. A *standalone* `ORDER BY total` is the alias on every dialect
        // (verified live), so it still expands to the projection expression, not the source column.
        let out = canon(
            total_view(Some("total"), vec![bare("total")]),
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
        );
        assert_eq!(out.order_by[0].expr, amount_times_two());
    }

    #[test]
    fn nested_order_by_name_colliding_with_a_source_column_rebinds_to_the_source() {
        // `ORDER BY total + 0` with a source column `total`: every dialect resolves the *nested* reference to
        // the source column (MySQL deparses `t.total + 0`), so it rebinds to `q.total`, not the alias.
        let nested = ExprNode::Binary {
            op: ArithmeticOp::Add,
            left: Box::new(bare("total")),
            right: Box::new(lit("0")),
        };
        let out = canon(
            total_view(Some("total"), vec![nested]),
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
        );
        assert_eq!(
            out.order_by[0].expr,
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(q_col("total")),
                right: Box::new(lit("0")),
            }
        );
        // The internal alias no clause references is pruned.
        assert_eq!(out.projection[0].internal_alias, None);
    }

    #[test]
    fn nested_pure_alias_without_a_source_collision_expands() {
        // `ORDER BY total + 0` with NO source column `total` (MySQL/SQLite allow a nested alias here): the
        // reference is the alias, so it expands.
        let nested = ExprNode::Binary {
            op: ArithmeticOp::Add,
            left: Box::new(bare("total")),
            right: Box::new(lit("0")),
        };
        let out = canon(
            total_view(Some("total"), vec![nested]),
            &QCatalog(vec!["amount".to_owned()]),
        );
        assert_eq!(
            out.order_by[0].expr,
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(amount_times_two()),
                right: Box::new(lit("0")),
            }
        );
    }

    #[test]
    fn a_standalone_source_column_order_by_is_left_as_a_bound_column() {
        // `ORDER BY amount` names a source column, not an alias — it binds to `q.amount` (idempotent: the
        // desired side already carries `Column`, the introspected bare form converges to it).
        let out = canon(
            total_view(None, vec![bare("amount")]),
            &QCatalog(vec!["amount".to_owned()]),
        );
        assert_eq!(out.order_by[0].expr, q_col("amount"));
    }

    #[test]
    fn an_already_expanded_introspected_clause_is_unchanged() {
        // The introspected side already carries the expression (the deparser expanded it): the pass is a
        // no-op, so it converges with the expanded desired side.
        let out = canon(
            total_view(None, vec![amount_times_two()]),
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
        );
        assert_eq!(out.order_by[0].expr, amount_times_two());
    }

    #[test]
    fn adversarial_two_distinct_views_do_not_collapse() {
        // A view ordering by the ALIAS and one ordering by a genuine SOURCE column `total` must NOT
        // canonicalize to the same body — else a real change would be a false-EQUAL and skip a replacement.
        let by_alias = canon(
            total_view(Some("total"), vec![bare("total")]),
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
        );
        // The second view genuinely orders by the source column (nested, so it is the source).
        let by_source_nested = canon(
            total_view(
                Some("total"),
                vec![ExprNode::Binary {
                    op: ArithmeticOp::Add,
                    left: Box::new(bare("total")),
                    right: Box::new(lit("0")),
                }],
            ),
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
        );
        assert_ne!(by_alias.order_by[0].expr, by_source_nested.order_by[0].expr);
    }

    #[test]
    fn group_by_and_having_alias_references_expand() {
        let mut query = total_view(Some("total"), Vec::new());
        query.group_by = vec![bare("total")];
        query.having = Some(bare("total"));
        let out = canon(query, &QCatalog(vec!["amount".to_owned()]));
        assert_eq!(out.group_by[0], amount_times_two());
        assert_eq!(out.having, Some(amount_times_two()));
    }

    #[test]
    fn standalone_group_by_colliding_with_a_source_column_rebinds_to_source() {
        // Unlike `ORDER BY`, a standalone `GROUP BY total` colliding with a source column `total` resolves to
        // the SOURCE column on PostgreSQL/MySQL/SQLite (verified live) — it must rebind, not expand, or two
        // semantically different grouped views would collapse (false-equal).
        let mut query = total_view(Some("total"), Vec::new());
        query.group_by = vec![bare("total")];
        let out = canon(
            query,
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
        );
        assert_eq!(out.group_by[0], q_col("total"));
    }

    #[test]
    fn standalone_having_colliding_with_a_source_column_rebinds_to_source() {
        let mut query = total_view(Some("total"), Vec::new());
        query.having = Some(bare("total"));
        let out = canon(
            query,
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
        );
        assert_eq!(out.having, Some(q_col("total")));
    }

    #[test]
    fn a_column_list_output_name_is_not_a_referenceable_alias() {
        // A view always renders a `(<columns>)` list, suppressing each projection's `AS`; so an output name
        // with NO kept internal alias is not an in-scope alias. A clause bare name matching it is the source
        // column (rebind), never the projection expression.
        let mut query = total_view(None, vec![bare("total")]); // internal_alias: None, output_name: "total"
        query.order_by[0].expr = bare("total");
        let out = canon(
            query,
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
        );
        assert_eq!(out.order_by[0].expr, q_col("total"));
    }

    #[test]
    fn output_name_is_referenceable_only_without_a_column_list() {
        let cs = ClauseCtx {
            catalog: &QCatalog(vec![]),
            dialect: test_dialect(IdentifierCase::Sensitive, false),
        };
        // Column-listed scope (top-level view / `WITH cte (cols)`): output_name is NOT an in-scope alias.
        let computed = vec![ProjectionItem {
            output_name: "total".to_owned(),
            internal_alias: None,
            expr: amount_times_two(),
        }];
        assert!(referenceable_alias_expr(&computed, "total", true, &cs).is_none());
        // Non-column-listed scope (set arm / column-list-less CTE / derived table / subquery): it IS.
        assert_eq!(
            referenceable_alias_expr(&computed, "total", false, &cs),
            Some(&amount_times_two())
        );
        // A kept internal alias is referenceable regardless of the column list.
        let aliased = vec![ProjectionItem {
            output_name: "n".to_owned(),
            internal_alias: Some("total".to_owned()),
            expr: amount_times_two(),
        }];
        assert_eq!(
            referenceable_alias_expr(&aliased, "total", true, &cs),
            Some(&amount_times_two())
        );
        // A plain column named after itself is a source column, not an alias.
        let self_named = vec![ProjectionItem {
            output_name: "amount".to_owned(),
            internal_alias: None,
            expr: q_col("amount"),
        }];
        assert!(referenceable_alias_expr(&self_named, "amount", false, &cs).is_none());
        // Case-insensitive backend: an alias `Total` is referenceable by `total`.
        let ci = ClauseCtx {
            catalog: &QCatalog(vec![]),
            dialect: test_dialect(IdentifierCase::AsciiInsensitive, false),
        };
        let mixed = vec![ProjectionItem {
            output_name: "n".to_owned(),
            internal_alias: Some("Total".to_owned()),
            expr: amount_times_two(),
        }];
        assert_eq!(
            referenceable_alias_expr(&mixed, "total", true, &ci),
            Some(&amount_times_two())
        );
    }

    #[test]
    fn a_join_group_by_binds_to_the_unique_source_that_exposes_the_name() {
        // Two-source join `FROM t q JOIN u u`: `t` exposes `amount`, `u` exposes `total`. A `GROUP BY total`
        // colliding with the `total` alias resolves to the source column (GROUP BY), and exactly one source
        // (`u`) exposes it, so it binds to `u.total` — matching the deparser's qualified form.
        struct MultiCatalog;
        impl ViewSourceColumns for MultiCatalog {
            fn source_columns(&self, source: &SourceRef) -> Option<Vec<String>> {
                match source.name.as_str() {
                    "t" => Some(vec!["amount".to_owned()]),
                    "u" => Some(vec!["total".to_owned()]),
                    _ => None,
                }
            }
        }
        let query = ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: Some("total".to_owned()),
                expr: amount_times_two(),
            }],
            from: Some(q_source()),
            joins: vec![JoinItem {
                kind: JoinKind::Inner,
                source: SourceItem::Named(SourceRef {
                    schema: None,
                    name: "u".to_owned(),
                    alias: "u".to_owned(),
                }),
                on: None,
            }],
            group_by: vec![bare("total")],
            ..ViewQueryModel::default()
        };
        let out = canon(query, &MultiCatalog);
        assert_eq!(
            out.group_by[0],
            ExprNode::Column {
                alias: "u".to_owned(),
                column: "total".to_owned(),
            }
        );
    }

    #[test]
    fn an_unresolved_source_leaves_a_group_by_alias_bare() {
        // When a source cannot be resolved (an unknown/external relation, or a join scope), a `GROUP BY` /
        // nested name that might be a hidden source column is left bare rather than expanded — a harmless
        // re-plan, never a false-equal.
        struct EmptyCatalog;
        impl ViewSourceColumns for EmptyCatalog {
            fn source_columns(&self, _source: &SourceRef) -> Option<Vec<String>> {
                None
            }
        }
        let mut query = total_view(Some("total"), Vec::new());
        query.group_by = vec![bare("total")];
        let mut body = ViewBody::Select(Box::new(query));
        canonicalize_view_clause_aliases(
            &mut body,
            &EmptyCatalog,
            test_dialect(IdentifierCase::Sensitive, false),
            true,
        );
        let ViewBody::Select(query) = body else {
            unreachable!()
        };
        assert_eq!(query.group_by[0], bare("total"));
    }

    #[test]
    fn a_standalone_order_by_alias_expands_even_with_an_unresolved_source() {
        // A standalone `ORDER BY` alias wins any collision on every dialect, so it expands even when the
        // source is unresolved.
        struct EmptyCatalog;
        impl ViewSourceColumns for EmptyCatalog {
            fn source_columns(&self, _source: &SourceRef) -> Option<Vec<String>> {
                None
            }
        }
        let query = total_view(Some("total"), vec![bare("total")]);
        let mut body = ViewBody::Select(Box::new(query));
        canonicalize_view_clause_aliases(
            &mut body,
            &EmptyCatalog,
            test_dialect(IdentifierCase::Sensitive, false),
            true,
        );
        let ViewBody::Select(query) = body else {
            unreachable!()
        };
        assert_eq!(query.order_by[0].expr, amount_times_two());
    }

    #[test]
    fn case_sensitivity_governs_collision_detection() {
        // Source column `Total` (mixed case) vs projection alias `total`, nested `ORDER BY total + 0`.
        // MySQL/SQLite fold case: `total` collides with `Total` and rebinds to the source. PostgreSQL is
        // case-sensitive: they are distinct, so the alias expands.
        let nested = || ExprNode::Binary {
            op: ArithmeticOp::Add,
            left: Box::new(bare("total")),
            right: Box::new(lit("0")),
        };
        let run = |case_insensitive: bool| {
            let mut body = ViewBody::Select(Box::new(total_view(Some("total"), vec![nested()])));
            let case = if case_insensitive {
                IdentifierCase::AsciiInsensitive
            } else {
                IdentifierCase::Sensitive
            };
            canonicalize_view_clause_aliases(
                &mut body,
                &QCatalog(vec!["amount".to_owned(), "Total".to_owned()]),
                test_dialect(case, false),
                true,
            );
            let ViewBody::Select(query) = body else {
                unreachable!()
            };
            query.order_by[0].expr.clone()
        };
        // Case-insensitive (MySQL/SQLite): rebind to the source column, using the source's OWN spelling
        // (`Total`) — the form the deparser emits — not the clause reference's casing (`total`).
        assert_eq!(
            run(true),
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(ExprNode::Column {
                    alias: "q".to_owned(),
                    column: "Total".to_owned(),
                }),
                right: Box::new(lit("0")),
            }
        );
        // Case-sensitive (PostgreSQL): no collision, the alias expands.
        assert_eq!(
            run(false),
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(amount_times_two()),
                right: Box::new(lit("0")),
            }
        );
    }

    #[test]
    fn unicode_case_folding_detects_a_non_ascii_collision_on_mysql() {
        // MySQL folds the full Unicode range: a nested `ORDER BY total` where the source column is `TOTAL`
        // is caught by ASCII folding, but a source column `TÖTAL` vs clause `tötal` needs Unicode folding —
        // it must be recognised as a collision and rebound to the source, not expanded to the alias.
        let nested = ExprNode::Binary {
            op: ArithmeticOp::Add,
            left: Box::new(bare("tötal")),
            right: Box::new(lit("0")),
        };
        let mut body = ViewBody::Select(Box::new(ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "tötal".to_owned(),
                internal_alias: Some("tötal".to_owned()),
                expr: amount_times_two(),
            }],
            from: Some(q_source()),
            order_by: vec![OrderItem {
                expr: nested,
                direction: None,
                nulls: None,
            }],
            ..ViewQueryModel::default()
        }));
        canonicalize_view_clause_aliases(
            &mut body,
            &QCatalog(vec!["amount".to_owned(), "TÖTAL".to_owned()]),
            test_dialect(IdentifierCase::UnicodeInsensitive, false),
            true,
        );
        let ViewBody::Select(query) = body else {
            unreachable!()
        };
        // Unicode folding recognised `tötal` == `TÖTAL`, so the nested reference rebinds to the source column.
        assert_eq!(
            query.order_by[0].expr,
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(ExprNode::Column {
                    alias: "q".to_owned(),
                    column: "TÖTAL".to_owned(),
                }),
                right: Box::new(lit("0")),
            }
        );
    }

    #[test]
    fn a_non_recursive_cte_body_does_not_see_itself() {
        // `WITH c AS (SELECT (q.amount*2) AS total FROM t q ORDER BY total + 0)` — non-recursive, so `c` is
        // not in scope inside its own body; the nested `total` resolves against `t` (which has no `total`),
        // so it is the projection alias and expands. (A recursive CTE, whose body DOES see itself, is
        // exercised by the round-trip corpus.)
        let cte_body = ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: Some("total".to_owned()),
                expr: amount_times_two(),
            }],
            from: Some(q_source()),
            order_by: vec![OrderItem {
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Add,
                    left: Box::new(bare("total")),
                    right: Box::new(lit("0")),
                },
                direction: None,
                nulls: None,
            }],
            ..ViewQueryModel::default()
        };
        let mut body = ViewBody::With {
            recursive: false,
            ctes: vec![CteModel {
                name: "c".to_owned(),
                columns: vec!["total".to_owned()],
                body: ViewBody::Select(Box::new(cte_body)),
            }],
            body: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "total".to_owned(),
                    internal_alias: None,
                    expr: ExprNode::Column {
                        alias: "c".to_owned(),
                        column: "total".to_owned(),
                    },
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: "c".to_owned(),
                    alias: "c".to_owned(),
                })),
                ..ViewQueryModel::default()
            }))),
        };
        // `t` exposes only `amount` (no `total`), so the CTE body's nested `total` is the alias and expands.
        canonicalize_view_clause_aliases(
            &mut body,
            &QCatalog(vec!["amount".to_owned()]),
            test_dialect(IdentifierCase::Sensitive, false),
            true,
        );
        let ViewBody::With { ctes, .. } = &body else {
            unreachable!()
        };
        let ViewBody::Select(cte_query) = &ctes[0].body else {
            unreachable!()
        };
        assert_eq!(
            cte_query.order_by[0].expr,
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(amount_times_two()),
                right: Box::new(lit("0")),
            }
        );
    }

    #[test]
    fn a_forward_cte_reference_is_visible_only_when_the_dialect_allows_it() {
        // `WITH a AS (SELECT (bq.x*2) AS total FROM b bq ORDER BY total + 0), b (total) AS (...)` — CTE `a`
        // forward-references later sibling `b`, which exposes `total`. On SQLite (forward refs visible) the
        // nested `total` is `b`'s source column and rebinds to `bq.total`; on PostgreSQL/MySQL `b` is not
        // visible to `a`, so the source is unresolved and the name is left bare (a harmless re-plan).
        struct EmptyCatalog;
        impl ViewSourceColumns for EmptyCatalog {
            fn source_columns(&self, _source: &SourceRef) -> Option<Vec<String>> {
                None
            }
        }
        let make = |recursive: bool| {
            let a_body = ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "total".to_owned(),
                    internal_alias: Some("total".to_owned()),
                    expr: ExprNode::Binary {
                        op: ArithmeticOp::Multiply,
                        left: Box::new(ExprNode::Column {
                            alias: "bq".to_owned(),
                            column: "x".to_owned(),
                        }),
                        right: Box::new(lit("2")),
                    },
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: "b".to_owned(),
                    alias: "bq".to_owned(),
                })),
                order_by: vec![OrderItem {
                    expr: ExprNode::Binary {
                        op: ArithmeticOp::Add,
                        left: Box::new(bare("total")),
                        right: Box::new(lit("0")),
                    },
                    direction: None,
                    nulls: None,
                }],
                ..ViewQueryModel::default()
            };
            let b_body = ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "total".to_owned(),
                    internal_alias: None,
                    expr: ExprNode::Column {
                        alias: "tq".to_owned(),
                        column: "total".to_owned(),
                    },
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: "t".to_owned(),
                    alias: "tq".to_owned(),
                })),
                ..ViewQueryModel::default()
            };
            ViewBody::With {
                recursive,
                ctes: vec![
                    CteModel {
                        name: "a".to_owned(),
                        columns: Vec::new(),
                        body: ViewBody::Select(Box::new(a_body)),
                    },
                    CteModel {
                        name: "b".to_owned(),
                        columns: vec!["total".to_owned()],
                        body: ViewBody::Select(Box::new(b_body)),
                    },
                ],
                body: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                    projection: vec![ProjectionItem {
                        output_name: "total".to_owned(),
                        internal_alias: None,
                        expr: ExprNode::Column {
                            alias: "aq".to_owned(),
                            column: "total".to_owned(),
                        },
                    }],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: None,
                        name: "a".to_owned(),
                        alias: "aq".to_owned(),
                    })),
                    ..ViewQueryModel::default()
                }))),
            }
        };
        let a_order = |recursive: bool, forward_flag: bool, recursive_forward: bool| {
            let mut body = make(recursive);
            canonicalize_view_clause_aliases(
                &mut body,
                &EmptyCatalog,
                ViewClauseDialect {
                    identifier_case: IdentifierCase::AsciiInsensitive,
                    cte_forward_references_visible: forward_flag,
                    recursive_exposes_forward_ctes: recursive_forward,
                },
                true,
            );
            let ViewBody::With { ctes, .. } = body else {
                unreachable!()
            };
            let ViewBody::Select(a) = &ctes[0].body else {
                unreachable!()
            };
            a.order_by[0].expr.clone()
        };
        let rebound = ExprNode::Binary {
            op: ArithmeticOp::Add,
            left: Box::new(ExprNode::Column {
                alias: "bq".to_owned(),
                column: "total".to_owned(),
            }),
            right: Box::new(lit("0")),
        };
        let left_bare = ExprNode::Binary {
            op: ArithmeticOp::Add,
            left: Box::new(bare("total")),
            right: Box::new(lit("0")),
        };
        // SQLite (plain `WITH` forward refs visible): the nested `total` is `b.total`, rebound to `bq.total`.
        assert_eq!(a_order(false, true, false), rebound);
        // PostgreSQL/MySQL plain `WITH` (no forward refs): `b` is a HIDDEN local CTE, so it resolves to
        // nothing (not a same-named catalog table) and `total` is left bare — a harmless re-plan, never wrong DDL.
        assert_eq!(a_order(false, false, false), left_bare);
        // MySQL `WITH RECURSIVE` (does NOT expose later siblings): still hidden, so left bare.
        assert_eq!(a_order(true, false, false), left_bare);
        // PostgreSQL `WITH RECURSIVE` (DOES expose later siblings): `b` is visible, so it rebinds.
        assert_eq!(a_order(true, false, true), rebound);
    }

    #[test]
    fn a_column_list_less_view_treats_output_name_as_an_alias() {
        // A view with NO declared columns renders each projection `AS output_name`, so `ORDER BY total` names
        // the computed alias and expands (top_level_column_listed = false) — even though `total` is also a
        // source column, a standalone `ORDER BY` alias wins.
        let query = total_view(None, vec![bare("total")]);
        let mut body = ViewBody::Select(Box::new(query));
        canonicalize_view_clause_aliases(
            &mut body,
            &QCatalog(vec!["amount".to_owned(), "total".to_owned()]),
            test_dialect(IdentifierCase::Sensitive, false),
            false, // no declared column list
        );
        let ViewBody::Select(query) = body else {
            unreachable!()
        };
        assert_eq!(query.order_by[0].expr, amount_times_two());
    }

    #[test]
    fn expanding_an_alias_binds_its_bare_source_columns() {
        // Projection `(b + 1) AS a` carrying a BARE source column `b` (a hand-built/KDL body); a standalone
        // `ORDER BY a` expands to `b + 1`, and the inserted `b` must bind to `q.b` so the canonical form is
        // fully resolved (idempotent with the deparser's qualified output).
        let query = ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "a".to_owned(),
                internal_alias: Some("a".to_owned()),
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Add,
                    left: Box::new(bare("b")),
                    right: Box::new(lit("1")),
                },
            }],
            from: Some(q_source()),
            order_by: vec![OrderItem {
                expr: bare("a"),
                direction: None,
                nulls: None,
            }],
            ..ViewQueryModel::default()
        };
        let out = canon(query, &QCatalog(vec!["b".to_owned()]));
        assert_eq!(
            out.order_by[0].expr,
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(q_col("b")),
                right: Box::new(lit("1")),
            }
        );
    }

    #[test]
    fn a_raw_clause_retains_internal_aliases() {
        // An opaque `Raw` clause (a legacy-package or hand-built body) may reference an alias by name the
        // structural scan cannot see; pruning must conservatively retain every internal alias.
        let mut query = total_view(Some("total"), Vec::new());
        query.order_by = vec![OrderItem {
            expr: ExprNode::Raw("total".to_owned()),
            direction: None,
            nulls: None,
        }];
        let out = canon(query, &QCatalog(vec!["amount".to_owned()]));
        assert_eq!(out.projection[0].internal_alias.as_deref(), Some("total"));
    }

    #[test]
    fn a_cte_lookup_is_case_insensitive_on_folding_backends() {
        // A CTE declared `C` referenced as `c` must still shadow the global catalog on MySQL/SQLite.
        struct EmptyCatalog;
        impl ViewSourceColumns for EmptyCatalog {
            fn source_columns(&self, _source: &SourceRef) -> Option<Vec<String>> {
                None
            }
        }
        let inner = ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: None,
                expr: q_col("x"),
            }],
            from: Some(q_source()),
            ..ViewQueryModel::default()
        };
        let outer = ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: Some("total".to_owned()),
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Multiply,
                    left: Box::new(ExprNode::Column {
                        alias: "c".to_owned(),
                        column: "total".to_owned(),
                    }),
                    right: Box::new(lit("2")),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "c".to_owned(), // referenced lower-case...
                alias: "c".to_owned(),
            })),
            order_by: vec![OrderItem {
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Add,
                    left: Box::new(bare("total")),
                    right: Box::new(lit("0")),
                },
                direction: None,
                nulls: None,
            }],
            ..ViewQueryModel::default()
        };
        let mut body = ViewBody::With {
            recursive: false,
            ctes: vec![CteModel {
                name: "C".to_owned(), // ...declared upper-case
                columns: vec!["total".to_owned()],
                body: ViewBody::Select(Box::new(inner)),
            }],
            body: Box::new(ViewBody::Select(Box::new(outer))),
        };
        // Case-insensitive backend: `c` resolves to CTE `C`, so nested `total` is its source column (rebind).
        canonicalize_view_clause_aliases(
            &mut body,
            &EmptyCatalog,
            test_dialect(IdentifierCase::AsciiInsensitive, false),
            true,
        );
        let ViewBody::With { body, .. } = &body else {
            unreachable!()
        };
        let ViewBody::Select(query) = body.as_ref() else {
            unreachable!()
        };
        assert_eq!(
            query.order_by[0].expr,
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(ExprNode::Column {
                    alias: "c".to_owned(),
                    column: "total".to_owned(),
                }),
                right: Box::new(lit("0")),
            }
        );
    }

    #[test]
    fn a_cte_source_column_shadows_the_global_catalog() {
        // `WITH c AS (...) SELECT (q.x * 2) AS total FROM c q ORDER BY total + 0` — the CTE `c` exposes a
        // `total` column, so the nested `total` is a source column and rebinds, even though the global
        // catalog has no such table.
        struct EmptyCatalog;
        impl ViewSourceColumns for EmptyCatalog {
            fn source_columns(&self, _source: &SourceRef) -> Option<Vec<String>> {
                None
            }
        }
        let inner = ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: None,
                expr: q_col("x"),
            }],
            from: Some(q_source()),
            ..ViewQueryModel::default()
        };
        let outer = ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: Some("total".to_owned()),
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Multiply,
                    left: Box::new(ExprNode::Column {
                        alias: "c".to_owned(),
                        column: "x".to_owned(),
                    }),
                    right: Box::new(lit("2")),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "c".to_owned(),
                alias: "c".to_owned(),
            })),
            order_by: vec![OrderItem {
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Add,
                    left: Box::new(bare("total")),
                    right: Box::new(lit("0")),
                },
                direction: None,
                nulls: None,
            }],
            ..ViewQueryModel::default()
        };
        let mut body = ViewBody::With {
            recursive: false,
            ctes: vec![CteModel {
                name: "c".to_owned(),
                columns: vec!["total".to_owned()],
                body: ViewBody::Select(Box::new(inner)),
            }],
            body: Box::new(ViewBody::Select(Box::new(outer))),
        };
        canonicalize_view_clause_aliases(
            &mut body,
            &EmptyCatalog,
            test_dialect(IdentifierCase::Sensitive, false),
            true,
        );
        let ViewBody::With { body, .. } = &body else {
            unreachable!()
        };
        let ViewBody::Select(query) = body.as_ref() else {
            unreachable!()
        };
        // The nested `total` rebound to the CTE source column `c.total`, not the projection expression.
        assert_eq!(
            query.order_by[0].expr,
            ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(ExprNode::Column {
                    alias: "c".to_owned(),
                    column: "total".to_owned(),
                }),
                right: Box::new(lit("0")),
            }
        );
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

    fn agg_pin(ty: SqlType) -> ExprNode {
        ExprNode::Aggregate {
            func: AggregateFunc::Sum,
            distinct: false,
            operand: Box::new(bare("x")),
            result: Some(ty),
        }
    }

    fn case_pin(ty: SqlType) -> ExprNode {
        ExprNode::Case {
            arms: vec![],
            else_: None,
            result: Some(ty),
        }
    }

    fn project(name: &str, expr: ExprNode) -> ViewQueryModel {
        ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: name.to_owned(),
                internal_alias: None,
                expr,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn map_result_pins_folds_every_nested_pin() {
        // A body exercising the recursion paths at once: a `WITH` wrapping a `UNION` whose left arm's
        // projection carries an aggregate pin, and whose right arm selects from a derived table whose
        // projection carries a `CASE` pin. `map_result_pins` must reach both.
        let left = ViewBody::Select(Box::new(project("s", agg_pin(SqlType::I8))));
        let right = ViewBody::Select(Box::new(ViewQueryModel {
            from: Some(SourceItem::Derived {
                query: Box::new(ViewBody::Select(Box::new(project(
                    "c",
                    case_pin(SqlType::I8),
                )))),
                alias: "d".to_owned(),
            }),
            ..project("c", bare("c"))
        }));
        let mut body = ViewBody::With {
            recursive: false,
            ctes: Vec::new(),
            body: Box::new(ViewBody::Set {
                op: ViewSetOp::Union,
                all: false,
                left: Box::new(left),
                right: Box::new(right),
                order_by: Vec::new(),
                limit: None,
                offset: None,
            }),
        };

        body.map_result_pins(&|ty| {
            if *ty == SqlType::I8 {
                SqlType::I16
            } else {
                ty.clone()
            }
        });

        let ViewBody::With { body, .. } = &body else {
            panic!("expected a WITH body")
        };
        let ViewBody::Set { left, right, .. } = body.as_ref() else {
            panic!("expected a Set body")
        };
        let ViewBody::Select(left) = left.as_ref() else {
            panic!("expected a Select left arm")
        };
        assert_eq!(left.projection[0].expr, agg_pin(SqlType::I16));
        let ViewBody::Select(right) = right.as_ref() else {
            panic!("expected a Select right arm")
        };
        let Some(SourceItem::Derived { query, .. }) = &right.from else {
            panic!("expected a derived FROM")
        };
        let ViewBody::Select(derived) = query.as_ref() else {
            panic!("expected a Select derived body")
        };
        assert_eq!(derived.projection[0].expr, case_pin(SqlType::I16));
    }

    fn named(schema: &str, name: &str, alias: &str) -> SourceItem {
        SourceItem::Named(SourceRef {
            schema: Some(schema.to_owned()),
            name: name.to_owned(),
            alias: alias.to_owned(),
        })
    }

    #[test]
    fn map_sources_flattens_every_nested_schema() {
        // A body exercising every recursion path at once: a `WITH` whose CTE selects from a qualified
        // source, wrapping a `UNION` whose left arm has a qualified `FROM` plus an `EXISTS` subquery over
        // another qualified source, and whose right arm selects from a derived table over a qualified
        // source. `map_sources` must reach and rewrite them all.
        let cte = CteModel {
            name: "c".to_owned(),
            columns: Vec::new(),
            body: ViewBody::Select(Box::new(ViewQueryModel {
                from: Some(named("app", "base", "q0_0")),
                ..project("b", bare("b"))
            })),
        };
        let left = ViewBody::Select(Box::new(ViewQueryModel {
            from: Some(named("app", "left", "q0_0")),
            filter: Some(ExprNode::Exists {
                negated: false,
                subquery: Box::new(ViewQueryModel {
                    from: Some(named("app", "sub", "q1_0")),
                    ..project("s", bare("s"))
                }),
            }),
            ..project("l", bare("l"))
        }));
        let right = ViewBody::Select(Box::new(ViewQueryModel {
            from: Some(SourceItem::Derived {
                query: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                    from: Some(named("app", "derived", "q2_0")),
                    ..project("d", bare("d"))
                }))),
                alias: "d".to_owned(),
            }),
            ..project("d", bare("d"))
        }));
        let mut body = ViewBody::With {
            recursive: false,
            ctes: vec![cte],
            body: Box::new(ViewBody::Set {
                op: ViewSetOp::Union,
                all: false,
                left: Box::new(left),
                right: Box::new(right),
                order_by: Vec::new(),
                limit: None,
                offset: None,
            }),
        };

        body.map_sources(&|source| source.schema = None);

        // Every reachable source — both set arms, the EXISTS subquery, and the derived table — now
        // carries `schema: None` (the schema-qualifier flatten a SQLite view diff needs). `collect_body_
        // sources` drops CTE-bound names, so it does not see the CTE's own body; that is asserted below.
        let mut sources = Vec::new();
        collect_body_sources(&body, &mut sources);
        assert!(!sources.is_empty());
        assert!(sources.iter().all(|source| source.schema.is_none()));
        // Confirm the CTE's own body (which the collector skips) was flattened too.
        let ViewBody::With { ctes, .. } = &body else {
            panic!("expected a WITH body")
        };
        let ViewBody::Select(cte_body) = &ctes[0].body else {
            panic!("expected a Select CTE body")
        };
        let Some(SourceItem::Named(source)) = &cte_body.from else {
            panic!("expected a named FROM in the CTE body")
        };
        assert_eq!(source.schema, None);
    }

    #[test]
    fn map_exprs_visits_every_nested_expression() {
        // A `WITH` over a `UNION` whose left arm carries a `LIKE` in its filter and whose right arm
        // selects from a derived table with a `LIKE` inside a projected `CASE`. `map_exprs` must reach and
        // rewrite both — the deeply-nested one proves it recurses past view-body-only nodes (`CASE`,
        // derived tables) that `fold_like_case_insensitivity` leaves untouched.
        let like = |ci: bool| ExprNode::Like {
            case_insensitive: ci,
            negated: false,
            operand: Box::new(bare("name")),
            pattern: Box::new(lit("'a%'")),
        };
        let left = ViewBody::Select(Box::new(ViewQueryModel {
            filter: Some(like(true)),
            ..project("n", bare("n"))
        }));
        let cased = ExprNode::Case {
            arms: vec![CaseArm {
                when: Box::new(like(true)),
                then: Box::new(lit("1")),
            }],
            else_: None,
            result: None,
        };
        let right = ViewBody::Select(Box::new(ViewQueryModel {
            from: Some(SourceItem::Derived {
                query: Box::new(ViewBody::Select(Box::new(project("c", cased)))),
                alias: "d".to_owned(),
            }),
            ..project("c", bare("c"))
        }));
        let mut body = ViewBody::With {
            recursive: false,
            ctes: Vec::new(),
            body: Box::new(ViewBody::Set {
                op: ViewSetOp::Union,
                all: false,
                left: Box::new(left),
                right: Box::new(right),
                order_by: Vec::new(),
                limit: None,
                offset: None,
            }),
        };

        body.map_exprs(&|expr| {
            if let ExprNode::Like {
                case_insensitive, ..
            } = expr
            {
                *case_insensitive = false;
            }
        });

        // Both `LIKE`s — the left arm's filter and the derived table's `CASE` arm — are now case-sensitive.
        let ViewBody::With { body, .. } = &body else {
            panic!("expected a WITH body")
        };
        let ViewBody::Set { left, right, .. } = body.as_ref() else {
            panic!("expected a Set body")
        };
        let ViewBody::Select(left) = left.as_ref() else {
            panic!("expected a Select left arm")
        };
        assert_eq!(left.filter, Some(like(false)));
        let ViewBody::Select(right) = right.as_ref() else {
            panic!("expected a Select right arm")
        };
        let Some(SourceItem::Derived { query, .. }) = &right.from else {
            panic!("expected a derived FROM")
        };
        let ViewBody::Select(derived) = query.as_ref() else {
            panic!("expected a Select derived body")
        };
        let ExprNode::Case { arms, .. } = &derived.projection[0].expr else {
            panic!("expected a CASE projection")
        };
        assert_eq!(*arms[0].when, like(false));
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
    fn map_cast_types_folds_every_general_cast_target() {
        // Every general cast's target folds via the supplied fn, recursing through the whole expression,
        // so a backend can canonicalize a structural desired cast to its representative on both sides.
        let mut expr = ExprNode::Compare {
            op: CompareOp::Equals,
            left: Box::new(ExprNode::Cast {
                operand: Box::new(bare("x")),
                ty: SqlType::I8,
            }),
            right: Box::new(ExprNode::Cast {
                operand: Box::new(lit("0")),
                ty: SqlType::I64,
            }),
        };
        // Fold `I8` to its representative `I16` (as PostgreSQL's `smallint` inverse does); leave the rest.
        map_cast_types(&mut expr, &|ty| match ty {
            SqlType::I8 => SqlType::I16,
            other => other.clone(),
        });
        let ExprNode::Compare { left, right, .. } = &expr else {
            panic!("expected a comparison");
        };
        assert!(matches!(
            **left,
            ExprNode::Cast {
                ty: SqlType::I16,
                ..
            }
        ));
        assert!(matches!(
            **right,
            ExprNode::Cast {
                ty: SqlType::I64,
                ..
            }
        ));
    }

    #[test]
    fn normalize_recurses_into_a_cast_operand() {
        // A general cast carries a full expression; a `BETWEEN` inside it must expand to its `AND` pair,
        // so `CAST((x BETWEEN 1 AND 2) AS boolean)` normalizes to the same node as the deparsed
        // `CAST((x >= 1 AND x <= 2) AS boolean)` rather than churning.
        let cast_between = ExprNode::Cast {
            operand: Box::new(ExprNode::Between {
                negated: false,
                operand: Box::new(bare("x")),
                low: Box::new(lit("1")),
                high: Box::new(lit("2")),
            }),
            ty: SqlType::Bool,
        };
        let deparsed = ExprNode::Cast {
            operand: Box::new(and(
                cmp(CompareOp::GreaterThanOrEquals, bare("x"), lit("1")),
                cmp(CompareOp::LessThanOrEquals, bare("x"), lit("2")),
            )),
            ty: SqlType::Bool,
        };
        assert_eq!(normalize_expr(&cast_between), deparsed);
        assert_eq!(normalize_expr(&cast_between), normalize_expr(&deparsed));
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
    fn with_scoping_drops_all_cte_named_sources_including_forward_refs() {
        // `WITH a AS (SELECT id FROM seed), seed AS (SELECT id FROM base) SELECT id FROM a`. Every CTE name
        // is a local binding for every body in the `WITH` and shadows any same-named external relation — so
        // the `seed` read inside `a` (a *forward* reference to the later CTE) is the CTE, not a real
        // relation, and is dropped; only `base` (the `seed` CTE's real source) survives. The main body's
        // `a` reference is likewise a local CTE binding and is dropped. So `referenced_sources` = {base}.
        let select_from = |name: &str| {
            ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "id".to_owned(),
                    internal_alias: None,
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
            !names.contains(&"seed"),
            "a forward reference to the later CTE `seed` is a local binding (shadowing any external) and \
             must be dropped: {names:?}",
        );
        assert!(
            names.contains(&"base"),
            "the `seed` CTE's real source: {names:?}"
        );
        assert!(
            !names.contains(&"a"),
            "the main body's reference to the CTE `a` must be dropped: {names:?}",
        );
    }
}
