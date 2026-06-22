use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Add, BitAnd, BitOr, Div, Mul, Not, Sub};

/// A structured SQL source alias used by generated query typestates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceAlias {
    depth: usize,
    index: usize,
}

impl SourceAlias {
    pub const fn new(depth: usize, index: usize) -> Self {
        Self { depth, index }
    }

    pub const fn depth(self) -> usize {
        self.depth
    }

    pub const fn index(self) -> usize {
        self.index
    }
}

impl fmt::Display for SourceAlias {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "q{}_{}", self.depth, self.index)
    }
}

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

/// Marker trait for Rust types that can participate in numeric SQL operations.
///
/// Non-numeric expressions intentionally do not expose numeric operators:
///
/// ```compile_fail
/// use squealy::Expr;
///
/// let left: Expr<'static, String> = Expr::lit(String::from("Ada"));
/// let right: Expr<'static, String> = Expr::lit(String::from("Lovelace"));
///
/// let _ = left.add(right);
/// ```
pub trait SqlNumber {}

macro_rules! impl_sql_number {
    ($($ty:ty),* $(,)?) => {
        $(impl SqlNumber for $ty {})*
    };
}

impl_sql_number!(i8, i16, i32, i64, i128, isize);
impl_sql_number!(u8, u16, u32, u64, u128, usize);
impl_sql_number!(f32, f64);

/// Marker trait for Rust value types that can participate in SQL pattern matching (`LIKE` /
/// `ILIKE`). Only string-valued expressions expose the `like` family, so a pattern match against a
/// numeric column is a compile error. Implemented for `String` (the value type of a text column);
/// nullable text columns (value type `Option<String>`) are intentionally excluded for now.
pub trait SqlText {}

impl SqlText for String {}

/// Computes the Rust value type produced by SQL division.
///
/// Division may produce fractional values even when both operands are integers,
/// so Squealy models division as producing `f64` rather than preserving the
/// operand type.
pub trait SqlDivide: SqlNumber {
    type Output: SqlNumber;
}

macro_rules! impl_sql_divide {
    ($($ty:ty),* $(,)?) => {
        $(impl SqlDivide for $ty {
            type Output = f64;
        })*
    };
}

impl_sql_divide!(i8, i16, i32, i64, i128, isize);
impl_sql_divide!(u8, u16, u32, u64, u128, usize);
impl_sql_divide!(f32, f64);

/// Computes the Rust value type produced by SQL `SUM`, and the SQL type the rendered `SUM(...)` is
/// cast to so the database returns that exact type.
///
/// A `SUM` widens to avoid overflow, mirroring the database's own result type: PostgreSQL returns
/// `bigint` for `SUM` over `smallint`/`integer` columns but `numeric` for `SUM` over `bigint` and
/// `numeric` columns. So operands Squealy stores in ≤ 32-bit columns sum to `i64`, while 64-bit and
/// wider integer operands sum to `i128` (a `bigint` cast would narrow PostgreSQL's `numeric` result
/// and error on a total above `i64::MAX`). The rendered aggregate is cast to
/// [`SUM_CAST`](Self::SUM_CAST) so the wire type always matches [`Output`](Self::Output).
///
/// (Squealy stores `u32` as `bigint`, so a `u32` sum is `numeric`/`i128` like the other 64-bit
/// operands, even though the operand itself fits in 32 bits.)
pub trait SqlSum: SqlNumber {
    type Output: SqlNumber;

    /// The SQL type the rendered `SUM(operand)` is cast to so it decodes as [`Output`](Self::Output).
    const SUM_CAST: crate::SqlType;
}

macro_rules! impl_sql_sum {
    ($($ty:ty => $output:ty : $cast:expr),* $(,)?) => {
        $(impl SqlSum for $ty {
            type Output = $output;
            const SUM_CAST: crate::SqlType = $cast;
        })*
    };
}

impl_sql_sum!(
    // `smallint`/`integer`-backed operands: PostgreSQL `SUM` is `bigint`.
    i8 => i64 : crate::SqlType::I64,
    i16 => i64 : crate::SqlType::I64,
    i32 => i64 : crate::SqlType::I64,
    u8 => i64 : crate::SqlType::I64,
    u16 => i64 : crate::SqlType::I64,
    // `bigint`/`numeric`-backed operands (or values that already exceed 64 bits): PostgreSQL `SUM`
    // is `numeric`, so widen to `i128` rather than narrowing back to a 64-bit `bigint` cast.
    i64 => i128 : crate::SqlType::I128,
    i128 => i128 : crate::SqlType::I128,
    isize => i128 : crate::SqlType::I128,
    u32 => i128 : crate::SqlType::I128,
    u64 => i128 : crate::SqlType::I128,
    // A single `u128` can exceed `i128::MAX`, so keep the sum unsigned (decoded from `numeric`)
    // rather than narrowing valid values into `i128`.
    u128 => u128 : crate::SqlType::U128,
    usize => i128 : crate::SqlType::I128,
    f32 => f64 : crate::SqlType::F64,
    f64 => f64 : crate::SqlType::F64,
);

/// The non-null scalar an aggregate operates on, looking through any number of `Option` layers.
///
/// SQL aggregates (`SUM`/`AVG`/`MIN`/`MAX`) ignore `NULL` inputs, so an aggregate over a nullable
/// operand — a `#[column]` typed `Option<T>`, or any column reached through a `LEFT JOIN`
/// (`Nullable<K>`, value `Option<T>`) — produces the same result type as the non-null `T`. This
/// trait maps the operand's value type to that scalar: `T` → `T`, and `Option<T>` → `T`'s scalar
/// (recursively, so a left-joined nullable column's `Option<Option<T>>` still resolves to `T`).
///
/// It is implemented for the built-in scalar value types PostgreSQL actually provides `MIN`/`MAX`
/// aggregates for and, by `#[derive(ColumnType)]`, for newtype wrappers, plus the blanket `Option`
/// impl below. `bool` and `uuid::Uuid` are intentionally excluded: PostgreSQL has no `min`/`max`
/// aggregate for them (only for numbers, strings, and date/time types), so `.min()`/`.max()` on
/// such a column is a compile error rather than a runtime `function min(uuid) does not exist`. They
/// remain orderable for `ORDER BY`, which is a separate capability.
pub trait AggregateScalar {
    /// The underlying non-null scalar value type.
    type Scalar;
}

impl<T> AggregateScalar for Option<T>
where
    T: AggregateScalar,
{
    type Scalar = T::Scalar;
}

/// Opts a value type into SQL `MIN`/`MAX` by implementing [`AggregateScalar`] (its own scalar).
///
/// The built-in numeric/string/date-time value types are covered already. Use this to enable
/// `MIN`/`MAX` on a `#[derive(ColumnType)]` newtype — which is **not** automatic, so a newtype over
/// a type PostgreSQL has no `min`/`max` aggregate for (`bool`, `uuid`, JSON, bytes, …) is excluded
/// by default. Only opt in newtypes whose column type the database can actually order:
///
/// ```
/// # use squealy::*;
/// #[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
/// struct UserId(i32);
/// squealy::impl_aggregate_scalar!(UserId);
/// ```
#[macro_export]
macro_rules! impl_aggregate_scalar {
    ($($ty:ty),* $(,)?) => {
        $(impl $crate::AggregateScalar for $ty {
            type Scalar = $ty;
        })*
    };
}

impl_aggregate_scalar!(i8, i16, i32, i64, i128, isize);
impl_aggregate_scalar!(u8, u16, u32, u64, u128, usize);
impl_aggregate_scalar!(f32, f64, String);

// Date/time types have PostgreSQL `min`/`max` aggregates; `uuid` does not, so it is excluded.
#[cfg(feature = "systemtime")]
impl_aggregate_scalar!(std::time::SystemTime);
#[cfg(feature = "time")]
impl_aggregate_scalar!(time::OffsetDateTime);
#[cfg(feature = "chrono")]
impl_aggregate_scalar!(chrono::DateTime<chrono::Utc>);

/// Type-level identity for a SQL expression.
pub trait ExprKind {
    type Value;
}

/// Compile-time witness that two column value types are identical.
///
/// The `Table` derive emits a `LocalType: SameValue<<ReferencedColumn as ExprKind>::Value>` bound for
/// each `references(...)` foreign key, so a foreign key whose column type does not match the
/// referenced column's type fails to compile. The only implementor is the reflexive blanket impl, so
/// `A: SameValue<B>` holds exactly when `A` and `B` are the same type.
#[doc(hidden)]
pub trait SameValue<T> {}

#[doc(hidden)]
impl<T> SameValue<T> for T {}

#[doc(hidden)]
pub trait ExprAst: Clone {
    type Params: crate::HList;
}

/// Backend-parameterized rendering for an expression AST node.
///
/// Split out from [`ExprAst`] so the backend-agnostic `Params` bound (used by the query
/// combinators) stays free of a backend, while literal nodes can carry a
/// `where K::Value: Encode<B>` bound that is only checked at render/execution time — the
/// mirror of how [`Decode<B>`](crate::Decode) is checked when a row is read.
#[doc(hidden)]
pub trait RenderAst<B>: ExprAst
where
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>;
}

#[doc(hidden)]
#[derive(Debug, PartialEq)]
pub struct ColumnExprAst<K> {
    alias: SourceAlias,
    column: Cow<'static, str>,
    _kind: PhantomData<K>,
}

#[doc(hidden)]
#[derive(Debug, PartialEq)]
pub struct LiteralExprAst<K>
where
    K: ExprKind,
{
    value: K::Value,
    _kind: PhantomData<K>,
}

#[doc(hidden)]
#[derive(Debug, PartialEq)]
pub struct ParamExprAst<K> {
    _kind: PhantomData<K>,
}

impl<K> Clone for ColumnExprAst<K> {
    fn clone(&self) -> Self {
        Self {
            alias: self.alias,
            column: self.column.clone(),
            _kind: PhantomData,
        }
    }
}

impl<K> Clone for LiteralExprAst<K>
where
    K: ExprKind,
    K::Value: Clone,
{
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            _kind: PhantomData,
        }
    }
}

impl<K> Clone for ParamExprAst<K> {
    fn clone(&self) -> Self {
        Self { _kind: PhantomData }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct BinaryExprAst<Left, Right> {
    left: Left,
    op: ArithmeticOp,
    right: Right,
}

impl<K> ExprAst for ColumnExprAst<K> {
    type Params = crate::HNil;
}

impl<K, B> RenderAst<B> for ColumnExprAst<K>
where
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_column(self.alias, &self.column)
    }
}

impl<K> ExprAst for LiteralExprAst<K>
where
    K: ExprKind,
    K::Value: Clone,
{
    type Params = crate::HNil;
}

impl<K, B> RenderAst<B> for LiteralExprAst<K>
where
    K: ExprKind,
    K::Value: Clone + crate::Encode<B>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_literal(&self.value)
    }
}

impl<K> ExprAst for ParamExprAst<K>
where
    K: ExprKind,
{
    type Params = crate::HCons<K::Value, crate::HNil>;
}

impl<K, B> RenderAst<B> for ParamExprAst<K>
where
    K: ExprKind,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_param()
    }
}

impl<Left, Right> ExprAst for BinaryExprAst<Left, Right>
where
    Left: ExprAst,
    Right: ExprAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;
}

impl<Left, Right, B> RenderAst<B> for BinaryExprAst<Left, Right>
where
    Left: RenderAst<B>,
    Right: RenderAst<B>,
    Left::Params: crate::HAppend<Right::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_binary(
            self.op,
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

/// A SQL aggregate function call (`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`) over a single operand. The
/// operand's parameters flow straight through. When `cast` is set the rendered call is wrapped in a
/// `CAST(... AS <type>)` so the database's result type matches the advertised Rust value type (e.g.
/// `SUM`/`AVG` whose native type would otherwise be `numeric`).
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct AggregateExprAst<Operand, const DISTINCT: bool = false> {
    func: AggregateFunc,
    cast: Option<crate::SqlType>,
    operand: Operand,
}

impl<Operand, const DISTINCT: bool> ExprAst for AggregateExprAst<Operand, DISTINCT>
where
    Operand: ExprAst,
{
    type Params = Operand::Params;
}

impl<Operand, B, const DISTINCT: bool> RenderAst<B> for AggregateExprAst<Operand, DISTINCT>
where
    Operand: RenderAst<B>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_aggregate(self.func, DISTINCT, self.cast.as_ref(), |visitor| {
            self.operand.visit(visitor)
        })
    }
}

// ===== Window functions =====

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

/// Empty terminator for a window `PARTITION BY` / `ORDER BY` list.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowNil;

/// One `PARTITION BY` expression, consed onto the rest of the list.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct WindowPartition<Ast, Rest> {
    ast: Ast,
    rest: Rest,
}

/// One `ORDER BY` term (expression + direction), consed onto the rest of the list.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOrder<Ast, Rest> {
    ast: Ast,
    dir: OrderDirection,
    rest: Rest,
}

/// The runtime params of a window list, concatenated head-to-tail (render order).
#[doc(hidden)]
pub trait WindowListParams {
    type Params: crate::HList;
}
impl WindowListParams for WindowNil {
    type Params = crate::HNil;
}
impl<Ast, Rest> WindowListParams for WindowPartition<Ast, Rest>
where
    Ast: ExprAst,
    Rest: WindowListParams,
    Ast::Params: crate::HAppend<Rest::Params>,
{
    type Params = <Ast::Params as crate::HAppend<Rest::Params>>::Output;
}
impl<Ast, Rest> WindowListParams for WindowOrder<Ast, Rest>
where
    Ast: ExprAst,
    Rest: WindowListParams,
    Ast::Params: crate::HAppend<Rest::Params>,
{
    type Params = <Ast::Params as crate::HAppend<Rest::Params>>::Output;
}

/// Backend-parameterized rendering of a window list: emits each element comma-separated (the
/// `PARTITION BY` / `ORDER BY` keyword is written by [`ExprVisitor::visit_window`]).
#[doc(hidden)]
pub trait RenderWindowList<B>: WindowListParams
where
    B: crate::Backend,
{
    /// Whether the list has at least one element (so the keyword should be emitted).
    const NON_EMPTY: bool;

    fn render<V>(&self, visitor: &mut V, first: &mut bool) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>;
}
impl<B> RenderWindowList<B> for WindowNil
where
    B: crate::Backend,
{
    const NON_EMPTY: bool = false;
    fn render<V>(&self, _visitor: &mut V, _first: &mut bool) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        Ok(())
    }
}
impl<Ast, Rest, B> RenderWindowList<B> for WindowPartition<Ast, Rest>
where
    Ast: RenderAst<B>,
    Rest: RenderWindowList<B>,
    Ast::Params: crate::HAppend<Rest::Params>,
    B: crate::Backend,
{
    const NON_EMPTY: bool = true;
    fn render<V>(&self, visitor: &mut V, first: &mut bool) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        if !*first {
            visitor.visit_window_separator()?;
        }
        *first = false;
        self.ast.visit(visitor)?;
        self.rest.render(visitor, first)
    }
}
impl<Ast, Rest, B> RenderWindowList<B> for WindowOrder<Ast, Rest>
where
    Ast: RenderAst<B>,
    Rest: RenderWindowList<B>,
    Ast::Params: crate::HAppend<Rest::Params>,
    B: crate::Backend,
{
    const NON_EMPTY: bool = true;
    fn render<V>(&self, visitor: &mut V, first: &mut bool) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        if !*first {
            visitor.visit_window_separator()?;
        }
        *first = false;
        self.ast.visit(visitor)?;
        visitor.visit_window_order_direction(self.dir)?;
        self.rest.render(visitor, first)
    }
}

// ===== Searched CASE expressions =====

/// Terminator of the `WHEN … THEN …` arm cons-list (mirrors [`WindowNil`]).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaseNil;

/// One `WHEN <pred> THEN <val>` arm, consed onto the rest.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct CaseWhen<PredAst, ValAst, Rest> {
    when: PredAst,
    then: ValAst,
    rest: Rest,
}

/// The "no `ELSE`" slot of a `CASE` (result is then nullable). Renders nothing.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoElse;

impl ExprAst for NoElse {
    type Params = crate::HNil;
}
impl<B> RenderAst<B> for NoElse
where
    B: crate::Backend,
{
    fn visit<V>(&self, _visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        Ok(())
    }
}
impl NonAggregateAst for NoElse {}
impl AstProjectionClass for NoElse {
    type Class = ConstantTerm;
}
impl ExprColumns for NoElse {
    type Columns = ColumnFree;
}

/// Append a `WHEN/THEN` arm to the tail of the arm list (mirrors [`AppendOrder`]), so arms render in
/// the order they were added.
#[doc(hidden)]
pub trait AppendArm<PredAst, ValAst> {
    type Output;
    fn append_arm(self, when: PredAst, then: ValAst) -> Self::Output;
}
impl<PredAst, ValAst> AppendArm<PredAst, ValAst> for CaseNil {
    type Output = CaseWhen<PredAst, ValAst, CaseNil>;
    fn append_arm(self, when: PredAst, then: ValAst) -> Self::Output {
        CaseWhen {
            when,
            then,
            rest: CaseNil,
        }
    }
}
impl<PredAst, ValAst, HPred, HVal, Rest> AppendArm<PredAst, ValAst> for CaseWhen<HPred, HVal, Rest>
where
    Rest: AppendArm<PredAst, ValAst>,
{
    type Output = CaseWhen<HPred, HVal, Rest::Output>;
    fn append_arm(self, when: PredAst, then: ValAst) -> Self::Output {
        CaseWhen {
            when: self.when,
            then: self.then,
            rest: self.rest.append_arm(when, then),
        }
    }
}

/// Marker for a non-empty arm list (at least one `WHEN`). Gates `otherwise`/`end` so an empty
/// `CASE END` (invalid SQL) cannot be built.
#[doc(hidden)]
pub trait NonEmptyArms {}
impl<PredAst, ValAst, Rest> NonEmptyArms for CaseWhen<PredAst, ValAst, Rest> {}

/// Runtime params of the arm list, concatenated `WHEN`-params ++ `THEN`-params per arm, in order.
#[doc(hidden)]
pub trait CaseArmsParams {
    type Params: crate::HList;
}
impl CaseArmsParams for CaseNil {
    type Params = crate::HNil;
}
impl<PredAst, ValAst, Rest> CaseArmsParams for CaseWhen<PredAst, ValAst, Rest>
where
    PredAst: PredicateAst,
    ValAst: ExprAst,
    Rest: CaseArmsParams,
    PredAst::Params: crate::HAppend<ValAst::Params>,
    <PredAst::Params as crate::HAppend<ValAst::Params>>::Output: crate::HAppend<Rest::Params>,
{
    type Params = <<PredAst::Params as crate::HAppend<ValAst::Params>>::Output as crate::HAppend<
        Rest::Params,
    >>::Output;
}

/// All arm predicates are aggregate-free and all `THEN` values are aggregate-free — so the whole
/// `CASE` may be used in `WHERE` (combined with the `ELSE` check on [`CaseExprAst`]).
#[doc(hidden)]
pub trait CaseArmsNonAggregate {}
impl CaseArmsNonAggregate for CaseNil {}
impl<PredAst, ValAst, Rest> CaseArmsNonAggregate for CaseWhen<PredAst, ValAst, Rest>
where
    PredAst: NonAggregatePredicate,
    ValAst: NonAggregateAst,
    Rest: CaseArmsNonAggregate,
{
}

/// Folds the arm `THEN` values' [terms](ConstantTerm) via [`CombineTerm`] (constant identity at the
/// tail), giving the value-term contributed by the arms.
#[doc(hidden)]
pub trait CaseArmsTerm {
    type Term;
}
impl CaseArmsTerm for CaseNil {
    type Term = ConstantTerm;
}
impl<PredAst, ValAst, Rest> CaseArmsTerm for CaseWhen<PredAst, ValAst, Rest>
where
    PredAst: PredicateTerm,
    ValAst: AstProjectionClass,
    Rest: CaseArmsTerm,
    // The arm's own term combines its `THEN` value with its `WHEN` condition's term (so an aggregate
    // in the condition makes the arm aggregate, and a bare column keeps its column dependency), then
    // folds with the remaining arms.
    <ValAst as AstProjectionClass>::Class: CombineTerm<<PredAst as PredicateTerm>::Term>,
    <<ValAst as AstProjectionClass>::Class as CombineTerm<<PredAst as PredicateTerm>::Term>>::Output:
        CombineTerm<Rest::Term>,
{
    type Term = <<<ValAst as AstProjectionClass>::Class as CombineTerm<
        <PredAst as PredicateTerm>::Term,
    >>::Output as CombineTerm<Rest::Term>>::Output;
}

/// Folds the arm predicate columns and `THEN` value columns via [`CombineColumns`] (for `HAVING`
/// validity: a bare column anywhere in the `CASE` makes it [`HasBareColumn`]).
#[doc(hidden)]
pub trait CaseArmsColumns {
    type Columns;
}
impl CaseArmsColumns for CaseNil {
    type Columns = ColumnFree;
}
impl<PredAst, ValAst, Rest> CaseArmsColumns for CaseWhen<PredAst, ValAst, Rest>
where
    PredAst: crate::PredicateColumns,
    ValAst: ExprColumns,
    Rest: CaseArmsColumns,
    <ValAst as ExprColumns>::Columns: CombineColumns<Rest::Columns>,
    <PredAst as crate::PredicateColumns>::Columns:
        CombineColumns<<<ValAst as ExprColumns>::Columns as CombineColumns<Rest::Columns>>::Output>,
{
    type Columns = <<PredAst as crate::PredicateColumns>::Columns as CombineColumns<
        <<ValAst as ExprColumns>::Columns as CombineColumns<Rest::Columns>>::Output,
    >>::Output;
}

/// Backend-parameterized rendering of the arm list: emits each `WHEN <pred> THEN <val>` via
/// [`ExprVisitor::visit_case_when`] / [`visit_case_then`](ExprVisitor::visit_case_then). Requires a
/// [`PredicateAstVisitor`] because the `WHEN` condition is a predicate.
#[doc(hidden)]
pub trait RenderCaseArms<B>: CaseArmsParams
where
    B: crate::Backend,
{
    /// Number of arms (lets a structural visitor like the view IR pair predicate/value nodes).
    const LEN: usize;

    /// Render each `WHEN <pred> THEN <value>` arm. `cast` (the result type, when set) wraps each
    /// `THEN` value in `CAST(<value> AS <cast>)` so an all-parameter branch is typeable.
    fn render<V>(&self, visitor: &mut V, cast: Option<&crate::SqlType>) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>;
}
impl<B> RenderCaseArms<B> for CaseNil
where
    B: crate::Backend,
{
    const LEN: usize = 0;
    fn render<V>(&self, _visitor: &mut V, _cast: Option<&crate::SqlType>) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        Ok(())
    }
}
impl<PredAst, ValAst, Rest, B> RenderCaseArms<B> for CaseWhen<PredAst, ValAst, Rest>
where
    PredAst: RenderPredicateAst<B>,
    ValAst: RenderAst<B>,
    Rest: RenderCaseArms<B>,
    PredAst::Params: crate::HAppend<ValAst::Params>,
    <PredAst::Params as crate::HAppend<ValAst::Params>>::Output: crate::HAppend<Rest::Params>,
    B: crate::Backend,
{
    const LEN: usize = 1 + Rest::LEN;
    fn render<V>(&self, visitor: &mut V, cast: Option<&crate::SqlType>) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_case_when()?;
        self.when.visit(visitor)?;
        visitor.visit_case_then()?;
        visitor.visit_case_value_open(cast)?;
        self.then.visit(visitor)?;
        visitor.visit_case_value_close(cast)?;
        self.rest.render(visitor, cast)
    }
}

/// A searched `CASE WHEN … THEN … [ELSE …] END` value expression. `Arms` is the `WHEN/THEN`
/// cons-list; `Else` is the `ELSE` value AST or [`NoElse`].
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct CaseExprAst<Arms, Else> {
    arms: Arms,
    else_ast: Option<Else>,
    /// The result type to `CAST` the whole `CASE` to. The builder sets this from the (type-level) result
    /// value type `T`, so that a `CASE` whose branches are all bind parameters still has a determinable
    /// type for the database (Postgres can't infer `CASE … THEN $1 ELSE $2 END` otherwise). Mirrors the
    /// aggregate `CAST` wrapper.
    result: Option<crate::SqlType>,
}

impl<Arms, Else> ExprAst for CaseExprAst<Arms, Else>
where
    Arms: CaseArmsParams + Clone,
    Else: ExprAst,
    Arms::Params: crate::HAppend<Else::Params>,
{
    type Params = <Arms::Params as crate::HAppend<Else::Params>>::Output;
}

impl<Arms, Else, B> RenderAst<B> for CaseExprAst<Arms, Else>
where
    Arms: RenderCaseArms<B> + Clone,
    Else: RenderAst<B>,
    Arms::Params: crate::HAppend<Else::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_case(&self.arms, self.else_ast.as_ref(), self.result.as_ref())
    }
}

impl<Arms, Else> NonAggregateAst for CaseExprAst<Arms, Else>
where
    Arms: CaseArmsNonAggregate,
    Else: NonAggregateAst,
{
}

impl<Arms, Else> AstProjectionClass for CaseExprAst<Arms, Else>
where
    Arms: CaseArmsTerm,
    Else: AstProjectionClass,
    <Arms as CaseArmsTerm>::Term: CombineTerm<<Else as AstProjectionClass>::Class>,
{
    type Class =
        <<Arms as CaseArmsTerm>::Term as CombineTerm<<Else as AstProjectionClass>::Class>>::Output;
}

impl<Arms, Else> ExprColumns for CaseExprAst<Arms, Else>
where
    Arms: CaseArmsColumns,
    Else: ExprColumns,
    <Arms as CaseArmsColumns>::Columns: CombineColumns<<Else as ExprColumns>::Columns>,
{
    type Columns = <<Arms as CaseArmsColumns>::Columns as CombineColumns<
        <Else as ExprColumns>::Columns,
    >>::Output;
}

/// Builder for a searched [`CASE`](case) expression. Add arms with [`when`](Self::when), then finish
/// with [`otherwise`](Self::otherwise) (non-null result) or [`end`](Self::end) (nullable, no `ELSE`).
pub struct CaseBuilder<'scope, T, Arms> {
    arms: Arms,
    _marker: PhantomData<(&'scope (), T)>,
}

/// Start a searched `CASE WHEN <pred> THEN <val> … [ELSE <val>] END` value expression. Every arm's
/// `THEN` value (and the `ELSE`) must share a value type `T`.
pub fn case<'scope, T>() -> CaseBuilder<'scope, T, CaseNil> {
    CaseBuilder {
        arms: CaseNil,
        _marker: PhantomData,
    }
}

impl<'scope, T, Arms> CaseBuilder<'scope, T, Arms> {
    /// Add a `WHEN <condition> THEN <value>` arm. The condition is a predicate (as `where_` takes); the
    /// value must have value type `T`.
    pub fn when<P, PredAst, E>(
        self,
        condition: Predicate<'scope, P, PredAst>,
        value: E,
    ) -> CaseBuilder<'scope, T, Arms::Output>
    where
        P: PredicateKind,
        PredAst: PredicateAst,
        E: IntoExpr<'scope>,
        E::Kind: ExprKind<Value = T>,
        Arms: AppendArm<PredAst, E::Ast>,
    {
        CaseBuilder {
            arms: self.arms.append_arm(condition.ast, value.into_expr().ast),
            _marker: PhantomData,
        }
    }
}

// `otherwise`/`end` require at least one `WHEN` arm (`Arms: NonEmptyArms`), so an empty `CASE END`
// (invalid SQL) cannot be built. The whole `CASE` is cast to `T`'s SQL type so all-parameter branches
// still have a determinable type for the database.
impl<'scope, T, Arms> CaseBuilder<'scope, T, Arms> {
    /// Finish with an `ELSE <value>` branch. With an `ELSE`, the result is the non-null value type `T`.
    pub fn otherwise<E>(self, value: E) -> Expr<'scope, T, CaseExprAst<Arms, E::Ast>>
    where
        Arms: NonEmptyArms,
        T: ExprKind + crate::HasColumnType,
        E: IntoExpr<'scope>,
        E::Kind: ExprKind<Value = T>,
        CaseExprAst<Arms, E::Ast>: ExprAst,
    {
        Expr {
            ast: CaseExprAst {
                arms: self.arms,
                else_ast: Some(value.into_expr().ast),
                result: Some(crate::SqlType::from(
                    <T as crate::HasColumnType>::COLUMN_TYPE,
                )),
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }

    /// Finish without an `ELSE`. An unmatched row yields SQL `NULL`, so the result is nullable
    /// ([`ScalarNullable<T>`], value `Option<T>`).
    pub fn end(self) -> Expr<'scope, ScalarNullable<T>, CaseExprAst<Arms, NoElse>>
    where
        Arms: NonEmptyArms,
        T: ExprKind + crate::HasColumnType,
        CaseExprAst<Arms, NoElse>: ExprAst,
    {
        Expr {
            ast: CaseExprAst {
                arms: self.arms,
                else_ast: None,
                result: Some(crate::SqlType::from(
                    <T as crate::HasColumnType>::COLUMN_TYPE,
                )),
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }
}

/// An expression AST node for a window function: `func(operand) OVER (PARTITION BY … ORDER BY …)`,
/// optionally wrapped in a `CAST` (used by aggregate-over to pin the widened result's wire type).
/// `operand` renders the function's arguments (nothing for `ROW_NUMBER()`); `partitions`/`orders`
/// are the `OVER` lists.
#[doc(hidden)]
#[derive(Clone)]
pub struct WindowExprAst<Operand, Parts, Ords> {
    func: WindowFunc,
    cast: Option<crate::SqlType>,
    operand: Operand,
    partitions: Parts,
    orders: Ords,
}

impl<Operand, Parts, Ords> ExprAst for WindowExprAst<Operand, Parts, Ords>
where
    Operand: ExprAst,
    Parts: WindowListParams + Clone,
    Ords: WindowListParams + Clone,
    Operand::Params: crate::HAppend<Parts::Params>,
    <Operand::Params as crate::HAppend<Parts::Params>>::Output: crate::HAppend<Ords::Params>,
{
    type Params = <<Operand::Params as crate::HAppend<Parts::Params>>::Output as crate::HAppend<
        Ords::Params,
    >>::Output;
}

impl<Operand, Parts, Ords, B> RenderAst<B> for WindowExprAst<Operand, Parts, Ords>
where
    Operand: RenderAst<B>,
    Parts: RenderWindowList<B> + Clone,
    Ords: RenderWindowList<B> + Clone,
    Operand::Params: crate::HAppend<Parts::Params>,
    <Operand::Params as crate::HAppend<Parts::Params>>::Output: crate::HAppend<Ords::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_window(
            self.func,
            self.cast.as_ref(),
            |visitor| self.operand.visit(visitor),
            <Parts as RenderWindowList<B>>::NON_EMPTY,
            |visitor| {
                let mut first = true;
                self.partitions.render(visitor, &mut first)
            },
            <Ords as RenderWindowList<B>>::NON_EMPTY,
            |visitor| {
                let mut first = true;
                self.orders.render(visitor, &mut first)
            },
        )
    }
}

// A window function yields one value per row (a scalar projection that needs no `GROUP BY`), so it
// is a `ColumnTerm`/`ScalarProjection` and may be selected alongside bare columns.
impl<Operand, Parts, Ords> AstProjectionClass for WindowExprAst<Operand, Parts, Ords> {
    type Class = ColumnTerm;
}
// Deliberately NOT `NonAggregateAst` (keeps windows out of `WHERE`/`GROUP BY`) and deliberately NOT
// `ExprColumns` (keeps them out of `HAVING`/whole-table-aggregate validity): a window function is
// evaluated after grouping, so the backends reject it in any of those clauses. Normal `SELECT`/
// `ORDER BY` classify via `AstProjectionClass` above, so they are unaffected.

/// Marker for an expression AST that is not a window function (recursive through [`BinaryExprAst`]).
/// Window functions are evaluated after the result rows are produced, so they are invalid in a
/// `RETURNING` clause; this marker (via [`ReturnableProjection`]) gates them out while still allowing
/// columns, literals, params, aggregates, arithmetic, and scalar subqueries. Notably *not*
/// implemented for [`WindowExprAst`].
#[doc(hidden)]
pub trait NonWindowAst {}
impl<K> NonWindowAst for ColumnExprAst<K> {}
impl<K> NonWindowAst for LiteralExprAst<K> where K: ExprKind {}
impl<K> NonWindowAst for ParamExprAst<K> {}
impl<Operand, const DISTINCT: bool> NonWindowAst for AggregateExprAst<Operand, DISTINCT> {}
impl<Sub> NonWindowAst for ScalarSubqueryExprAst<Sub> {}
impl<Left, Right> NonWindowAst for BinaryExprAst<Left, Right>
where
    Left: NonWindowAst,
    Right: NonWindowAst,
{
}

/// Marker for a projection valid in a `RETURNING` clause: it contains no window function.
/// Implemented for columns, bare values, expressions over [`NonWindowAst`], the unit projection,
/// tuples of returnable projections, and whole-table projections (via the `Table` derive).
#[doc(hidden)]
pub trait ReturnableProjection {}
impl<'scope, K, Ast> ReturnableProjection for Expr<'scope, K, Ast> where Ast: ExprAst + NonWindowAst {}
impl<'scope, K> ReturnableProjection for ColumnRef<'scope, K> {}

/// Empty operand for a no-argument window function (`ROW_NUMBER()`, `RANK()`, `DENSE_RANK()`):
/// renders nothing between the parentheses.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowNoArg;

impl ExprAst for WindowNoArg {
    type Params = crate::HNil;
}
impl<B> RenderAst<B> for WindowNoArg
where
    B: crate::Backend,
{
    fn visit<V>(&self, _visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        Ok(())
    }
}

/// A window operand: a column or scalar expression, used as a `PARTITION BY` term or as the value
/// argument of `LAG`/`LEAD`. Exposes both its kind (for typing a function result) and its AST.
#[doc(hidden)]
pub trait WindowOperand<'scope> {
    type Kind: ExprKind;
    type Ast: ExprAst;
    fn into_window_ast(self) -> Self::Ast;
}
impl<'scope, K> WindowOperand<'scope> for ColumnRef<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;
    type Ast = ColumnExprAst<K>;
    fn into_window_ast(self) -> Self::Ast {
        self.into_expr().ast
    }
}
impl<'scope, K, Ast> WindowOperand<'scope> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    type Kind = K;
    type Ast = Ast;
    fn into_window_ast(self) -> Self::Ast {
        self.ast
    }
}

/// Append one `PARTITION BY` term's AST to the end of a partition list.
#[doc(hidden)]
pub trait AppendPartition<Ast> {
    type Output;
    fn append_partition(self, ast: Ast) -> Self::Output;
}
impl<Ast> AppendPartition<Ast> for WindowNil {
    type Output = WindowPartition<Ast, WindowNil>;
    fn append_partition(self, ast: Ast) -> Self::Output {
        WindowPartition {
            ast,
            rest: WindowNil,
        }
    }
}
impl<Head, Rest, Ast> AppendPartition<Ast> for WindowPartition<Head, Rest>
where
    Rest: AppendPartition<Ast>,
{
    type Output = WindowPartition<Head, Rest::Output>;
    fn append_partition(self, ast: Ast) -> Self::Output {
        WindowPartition {
            ast: self.ast,
            rest: self.rest.append_partition(ast),
        }
    }
}

/// Append one `ORDER BY` term (AST + direction) to the end of an order list.
#[doc(hidden)]
pub trait AppendOrder<Ast> {
    type Output;
    fn append_order(self, ast: Ast, direction: OrderDirection) -> Self::Output;
}
impl<Ast> AppendOrder<Ast> for WindowNil {
    type Output = WindowOrder<Ast, WindowNil>;
    fn append_order(self, ast: Ast, direction: OrderDirection) -> Self::Output {
        WindowOrder {
            ast,
            dir: direction,
            rest: WindowNil,
        }
    }
}
impl<Head, Rest, Ast> AppendOrder<Ast> for WindowOrder<Head, Rest>
where
    Rest: AppendOrder<Ast>,
{
    type Output = WindowOrder<Head, Rest::Output>;
    fn append_order(self, ast: Ast, direction: OrderDirection) -> Self::Output {
        WindowOrder {
            ast: self.ast,
            dir: self.dir,
            rest: self.rest.append_order(ast, direction),
        }
    }
}

/// The `OVER (…)` specification, built inside an `.over(|w| …)` closure. Chain
/// [`partition_by`](Self::partition_by) and [`order_by`](Self::order_by); each call appends one
/// term, so `.partition_by(a).partition_by(b)` yields `PARTITION BY a, b`.
pub struct Window<'scope, Parts = WindowNil, Ords = WindowNil> {
    partitions: Parts,
    orders: Ords,
    _scope: PhantomData<&'scope ()>,
}

impl<'scope> Window<'scope, WindowNil, WindowNil> {
    fn new() -> Self {
        Self {
            partitions: WindowNil,
            orders: WindowNil,
            _scope: PhantomData,
        }
    }
}

impl<'scope, Parts, Ords> Window<'scope, Parts, Ords> {
    /// Add a `PARTITION BY` term (a column or scalar expression). The term may not itself be a window
    /// or aggregate (a window definition is evaluated per row), enforced by `NonAggregateAst`.
    pub fn partition_by<E>(self, key: E) -> Window<'scope, Parts::Output, Ords>
    where
        E: WindowOperand<'scope>,
        E::Ast: NonAggregateAst,
        Parts: AppendPartition<E::Ast>,
    {
        Window {
            partitions: self.partitions.append_partition(key.into_window_ast()),
            orders: self.orders,
            _scope: PhantomData,
        }
    }

    /// Add an `ORDER BY` term (`col.asc()` / `col.desc()`). The term may not itself be a window or
    /// aggregate (a window definition is evaluated per row), enforced by `NonAggregateAst`.
    pub fn order_by<K, Ast>(
        self,
        order: Order<'scope, K, Ast>,
    ) -> Window<'scope, Parts, Ords::Output>
    where
        Ast: ExprAst + NonAggregateAst,
        Ords: AppendOrder<Ast>,
    {
        Window {
            partitions: self.partitions,
            orders: self.orders.append_order(order.ast, order.direction),
            _scope: PhantomData,
        }
    }
}

impl<'scope, K, Operand> Expr<'scope, K, AggregateExprAst<Operand, false>>
where
    Operand: ExprAst,
{
    /// Make this aggregate `DISTINCT` — `COUNT(DISTINCT x)`, `SUM(DISTINCT x)`, etc. Deduplicates the
    /// operand values before aggregating. Not available on a window aggregate (`DISTINCT` is invalid
    /// with `OVER (…)`), and `.over()` is in turn unavailable once `.distinct()` has been applied.
    pub fn distinct(self) -> Expr<'scope, K, AggregateExprAst<Operand, true>> {
        Expr {
            ast: AggregateExprAst {
                func: self.ast.func,
                cast: self.ast.cast,
                operand: self.ast.operand,
            },
            project_alias: self.project_alias,
            _phantom: PhantomData,
        }
    }

    /// Turn this aggregate into a window function: `SUM(x) OVER (…)`. The result keeps the
    /// aggregate's value type but is a per-row scalar (no `GROUP BY` required); build the `OVER`
    /// clause with the `Window` handle (`.partition_by(...)`, `.order_by(...)`).
    pub fn over<F, Parts, Ords>(
        self,
        build: F,
    ) -> Expr<'scope, K, WindowExprAst<Operand, Parts, Ords>>
    where
        F: FnOnce(Window<'scope, WindowNil, WindowNil>) -> Window<'scope, Parts, Ords>,
        Parts: Clone,
        Ords: Clone,
        WindowExprAst<Operand, Parts, Ords>: ExprAst,
    {
        let window = build(Window::new());
        Expr {
            ast: WindowExprAst {
                func: WindowFunc::Aggregate(self.ast.func),
                cast: self.ast.cast,
                operand: self.ast.operand,
                partitions: window.partitions,
                orders: window.orders,
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }
}

/// A dedicated window function awaiting its `OVER (…)` clause (`OVER` is mandatory for a window
/// function). Call [`over`](Self::over) to complete it into an [`Expr`].
pub struct PendingWindow<'scope, K, Operand> {
    func: WindowFunc,
    cast: Option<crate::SqlType>,
    operand: Operand,
    _marker: PhantomData<(&'scope (), K)>,
}

impl<'scope, K, Operand> PendingWindow<'scope, K, Operand>
where
    Operand: ExprAst,
{
    /// Complete the window function with its `OVER (…)` clause.
    pub fn over<F, Parts, Ords>(
        self,
        build: F,
    ) -> Expr<'scope, K, WindowExprAst<Operand, Parts, Ords>>
    where
        F: FnOnce(Window<'scope, WindowNil, WindowNil>) -> Window<'scope, Parts, Ords>,
        Parts: Clone,
        Ords: Clone,
        WindowExprAst<Operand, Parts, Ords>: ExprAst,
    {
        let window = build(Window::new());
        Expr {
            ast: WindowExprAst {
                func: self.func,
                cast: self.cast,
                operand: self.operand,
                partitions: window.partitions,
                orders: window.orders,
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }
}

/// The `ROW_NUMBER()` window function (sequential row number within the window). Returns `i64`.
pub fn row_number<'scope>() -> PendingWindow<'scope, i64, WindowNoArg> {
    PendingWindow {
        func: WindowFunc::RowNumber,
        cast: None,
        operand: WindowNoArg,
        _marker: PhantomData,
    }
}

/// The `RANK()` window function (rank with gaps after ties). Returns `i64`.
pub fn rank<'scope>() -> PendingWindow<'scope, i64, WindowNoArg> {
    PendingWindow {
        func: WindowFunc::Rank,
        cast: None,
        operand: WindowNoArg,
        _marker: PhantomData,
    }
}

/// The `DENSE_RANK()` window function (rank without gaps). Returns `i64`.
pub fn dense_rank<'scope>() -> PendingWindow<'scope, i64, WindowNoArg> {
    PendingWindow {
        func: WindowFunc::DenseRank,
        cast: None,
        operand: WindowNoArg,
        _marker: PhantomData,
    }
}

/// The `NTILE(buckets)` window function (assigns rows to `buckets` ranked groups). Returns `i32`.
pub fn ntile<'scope>(buckets: i32) -> PendingWindow<'scope, i32, LiteralExprAst<i32>> {
    PendingWindow {
        func: WindowFunc::Ntile,
        cast: None,
        operand: LiteralExprAst {
            value: buckets,
            _kind: PhantomData,
        },
        _marker: PhantomData,
    }
}

/// The arguments of `LAG`/`LEAD`: a value expression and an integer offset, rendered `value, offset`.
/// The offset is an `i32` (SQL `integer`): PostgreSQL's `lag`/`lead` take `int4`, so a `BIGINT`
/// offset would be a parameter-type mismatch.
#[doc(hidden)]
#[derive(Clone)]
pub struct LagArgsAst<ValueAst> {
    value: ValueAst,
    offset: i32,
}

impl<ValueAst> ExprAst for LagArgsAst<ValueAst>
where
    ValueAst: ExprAst,
{
    type Params = ValueAst::Params;
}

impl<ValueAst, B> RenderAst<B> for LagArgsAst<ValueAst>
where
    ValueAst: RenderAst<B>,
    B: crate::Backend,
    i32: crate::Encode<B>,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        self.value.visit(visitor)?;
        visitor.visit_window_separator()?;
        visitor.visit_literal(&self.offset)
    }
}

/// The `LAG(value, offset)` window function (the row `offset` rows before the current one in window
/// order). The result is nullable (`NULL` past the partition edge), so it decodes as `Option<T>`.
/// The value must be a row-level scalar (`NonAggregateAst`): a nested window or aggregate operand is
/// rejected by the backends.
pub fn lag<'scope, E>(
    value: E,
    offset: i32,
) -> PendingWindow<'scope, <E::Kind as IntoWindowNullable>::Kind, LagArgsAst<E::Ast>>
where
    E: WindowOperand<'scope>,
    E::Ast: NonAggregateAst,
    E::Kind: IntoWindowNullable,
{
    PendingWindow {
        func: WindowFunc::Lag,
        cast: None,
        operand: LagArgsAst {
            value: value.into_window_ast(),
            offset,
        },
        _marker: PhantomData,
    }
}

/// The `LEAD(value, offset)` window function (the row `offset` rows after the current one). Nullable
/// past the partition edge; see [`lag`].
pub fn lead<'scope, E>(
    value: E,
    offset: i32,
) -> PendingWindow<'scope, <E::Kind as IntoWindowNullable>::Kind, LagArgsAst<E::Ast>>
where
    E: WindowOperand<'scope>,
    E::Ast: NonAggregateAst,
    E::Kind: IntoWindowNullable,
{
    PendingWindow {
        func: WindowFunc::Lead,
        cast: None,
        operand: LagArgsAst {
            value: value.into_window_ast(),
            offset,
        },
        _marker: PhantomData,
    }
}

/// Marker for expression ASTs that are *not* a SQL aggregate function call (`COUNT`/`SUM`/…) and do
/// not contain one. It is implemented for every expression AST node except [`AggregateExprAst`]
/// (recursively for [`BinaryExprAst`]), so an aggregate cannot satisfy it.
///
/// Predicate ASTs built from aggregate-free operands are [`NonAggregatePredicate`], which `where_`
/// requires — so an aggregate cannot flow into a `WHERE` clause (PostgreSQL/MySQL reject aggregates
/// there; they belong in the select list, or `HAVING` once it is supported). Comparing an aggregate
/// is still possible; the resulting predicate just cannot be used as a `where_` filter.
pub trait NonAggregateAst {}

impl<K> NonAggregateAst for ColumnExprAst<K> {}
impl<K> NonAggregateAst for LiteralExprAst<K> where K: ExprKind {}
impl<K> NonAggregateAst for ParamExprAst<K> {}
impl<Left, Right> NonAggregateAst for BinaryExprAst<Left, Right>
where
    Left: NonAggregateAst,
    Right: NonAggregateAst,
{
}

/// Whether an expression references a *bare* (ungrouped) column: the dual axis to
/// [`NonAggregateAst`], used to validate `HAVING`. A bare column is only valid in a `HAVING` once a
/// `GROUP BY` is present; otherwise the query is a whole-table aggregate and the column belongs to
/// no group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnFree {}
/// See [`ColumnFree`]: the expression contains at least one bare (ungrouped) column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HasBareColumn {}

/// Combines two column classes ([`ColumnFree`] / [`HasBareColumn`]); `HasBareColumn` dominates.
#[doc(hidden)]
pub trait CombineColumns<Rhs> {
    type Output;
}
impl CombineColumns<ColumnFree> for ColumnFree {
    type Output = ColumnFree;
}
impl CombineColumns<HasBareColumn> for ColumnFree {
    type Output = HasBareColumn;
}
impl CombineColumns<ColumnFree> for HasBareColumn {
    type Output = HasBareColumn;
}
impl CombineColumns<HasBareColumn> for HasBareColumn {
    type Output = HasBareColumn;
}

/// Classifies an expression AST as [`ColumnFree`] or [`HasBareColumn`]. A column is bare unless it
/// sits inside an aggregate (`AggregateExprAst`), which collapses its operand to a single value.
#[doc(hidden)]
pub trait ExprColumns {
    type Columns;
}
impl<K> ExprColumns for ColumnExprAst<K> {
    type Columns = HasBareColumn;
}
impl<Operand, const DISTINCT: bool> ExprColumns for AggregateExprAst<Operand, DISTINCT> {
    type Columns = ColumnFree;
}
impl<K> ExprColumns for LiteralExprAst<K>
where
    K: ExprKind,
{
    type Columns = ColumnFree;
}
impl<K> ExprColumns for ParamExprAst<K> {
    type Columns = ColumnFree;
}
impl<Left, Right> ExprColumns for BinaryExprAst<Left, Right>
where
    Left: ExprColumns,
    Right: ExprColumns,
    <Left as ExprColumns>::Columns: CombineColumns<<Right as ExprColumns>::Columns>,
{
    type Columns =
        <<Left as ExprColumns>::Columns as CombineColumns<<Right as ExprColumns>::Columns>>::Output;
}

/// Classification of a projection element as a plain scalar value (see [`ProjectionClass`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarProjection {}

/// Classification of a projection element as a SQL aggregate (`COUNT`/`SUM`/…, see
/// [`ProjectionClass`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateProjection {}

/// Classifies a projection element (or a tuple of them) as scalar or aggregate so the query builder
/// can keep SQL valid without `GROUP BY`:
///
/// - `select` requires a *homogeneous* projection — every element the same class — so a list mixing
///   a bare column with an aggregate (`(user.id, user.id.count())`) has no impl and fails.
/// - `RETURNING` and update assignments require [`ScalarProjection`].
///
/// Classification is structural over expression [terms](ConstantTerm) (constant/param, bare column,
/// aggregate). An aggregate combined with a bare column (`COUNT(id) + id`, ungrouped) has no valid
/// [`CombineTerm`] and so is rejected everywhere.
#[doc(hidden)]
pub trait ProjectionClass {
    type Class;
}

/// A term of an expression for aggregate-validity: a constant/param.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstantTerm {}
/// A term of an expression for aggregate-validity: a bare (ungrouped) column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnTerm {}
/// A term of an expression for aggregate-validity: a SQL aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateTerm {}

/// Classifies an expression AST into a [term](ConstantTerm): constant, column, or aggregate.
#[doc(hidden)]
pub trait AstProjectionClass {
    type Class;
}

impl<Operand, const DISTINCT: bool> AstProjectionClass for AggregateExprAst<Operand, DISTINCT> {
    type Class = AggregateTerm;
}
impl<K> AstProjectionClass for ColumnExprAst<K> {
    type Class = ColumnTerm;
}
impl<K> AstProjectionClass for LiteralExprAst<K>
where
    K: ExprKind,
{
    type Class = ConstantTerm;
}
impl<K> AstProjectionClass for ParamExprAst<K> {
    type Class = ConstantTerm;
}
// A binary expression's term is its operands combined; `CombineTerm` has no impl for an aggregate
// mixed with a bare column (`COUNT(id) + id`), so such an expression is rejected from projections.
impl<Left, Right> AstProjectionClass for BinaryExprAst<Left, Right>
where
    Left: AstProjectionClass,
    Right: AstProjectionClass,
    <Left as AstProjectionClass>::Class: CombineTerm<<Right as AstProjectionClass>::Class>,
{
    type Class = <<Left as AstProjectionClass>::Class as CombineTerm<
        <Right as AstProjectionClass>::Class,
    >>::Output;
}

/// Combines two expression [terms](ConstantTerm): a constant is absorbed by either side; two
/// columns stay a column; an aggregate with a constant stays aggregate. An aggregate combined with
/// a bare column is ungrouped and invalid, so it has no impl.
#[doc(hidden)]
pub trait CombineTerm<Rhs> {
    type Output;
}
impl CombineTerm<ConstantTerm> for ConstantTerm {
    type Output = ConstantTerm;
}
impl CombineTerm<ColumnTerm> for ConstantTerm {
    type Output = ColumnTerm;
}
impl CombineTerm<AggregateTerm> for ConstantTerm {
    type Output = AggregateTerm;
}
impl CombineTerm<ConstantTerm> for ColumnTerm {
    type Output = ColumnTerm;
}
impl CombineTerm<ColumnTerm> for ColumnTerm {
    type Output = ColumnTerm;
}
impl CombineTerm<ConstantTerm> for AggregateTerm {
    type Output = AggregateTerm;
}
impl CombineTerm<AggregateTerm> for AggregateTerm {
    type Output = AggregateTerm;
}
// No `CombineTerm` between `ColumnTerm` and `AggregateTerm`: a bare column outside an aggregate is
// invalid without `GROUP BY`.

/// Maps an expression [term](ConstantTerm) to its projection class: constants and columns are
/// [`ScalarProjection`], aggregates are [`AggregateProjection`].
#[doc(hidden)]
pub trait TermProjectionClass {
    type Class;
}
impl TermProjectionClass for ConstantTerm {
    type Class = ScalarProjection;
}
impl TermProjectionClass for ColumnTerm {
    type Class = ScalarProjection;
}
impl TermProjectionClass for AggregateTerm {
    type Class = AggregateProjection;
}

impl<'scope, K, Ast> ProjectionClass for Expr<'scope, K, Ast>
where
    Ast: ExprAst + AstProjectionClass,
    <Ast as AstProjectionClass>::Class: TermProjectionClass,
{
    type Class = <<Ast as AstProjectionClass>::Class as TermProjectionClass>::Class;
}

impl<'scope, K> ProjectionClass for ColumnRef<'scope, K> {
    type Class = ScalarProjection;
}

/// Classifies a whole projection as [`ColumnFree`] or [`HasBareColumn`], independent of the
/// homogeneity that [`ProjectionClass`] requires. A whole-table aggregate (a `HAVING` with no
/// `GROUP BY`, the [`Aggregated`] state) requires a column-free projection: aggregates and constants
/// are fine (they do not depend on an ungrouped row), bare columns are not. Tuples implement it only
/// when every element is column-free.
#[doc(hidden)]
pub trait ProjectionColumns {
    type Columns;
}

impl<'scope, K, Ast> ProjectionColumns for Expr<'scope, K, Ast>
where
    Ast: ExprAst + ExprColumns,
{
    type Columns = <Ast as ExprColumns>::Columns;
}

impl<'scope, K> ProjectionColumns for ColumnRef<'scope, K> {
    type Columns = HasBareColumn;
}

// === ORDER BY classification ===

/// Order-class of a select chain (carried as [`SelectAst::OrderClass`](crate::SelectAst::OrderClass)):
/// which kinds of `ORDER BY` terms it has. `select` requires the ordering match the projection — an
/// aggregate-only query may order only by aggregates, a scalar query only by scalar columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderNone {}
/// Order-class: orders by at least one scalar (ungrouped) column and no aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderScalar {}
/// Order-class: orders by at least one aggregate and no scalar column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderAggregate {}
/// Order-class: orders by both a scalar column and an aggregate — never valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderMixed {}

/// Extends an order-class with one more `ORDER BY` term of the given [term](ConstantTerm) class.
#[doc(hidden)]
pub trait ExtendOrderClass<TermClass> {
    type Output;
}
// A constant order term (e.g. a bound param) constrains nothing.
impl ExtendOrderClass<ConstantTerm> for OrderNone {
    type Output = OrderNone;
}
impl ExtendOrderClass<ConstantTerm> for OrderScalar {
    type Output = OrderScalar;
}
impl ExtendOrderClass<ConstantTerm> for OrderAggregate {
    type Output = OrderAggregate;
}
impl ExtendOrderClass<ConstantTerm> for OrderMixed {
    type Output = OrderMixed;
}
// A column order term introduces a scalar dependency.
impl ExtendOrderClass<ColumnTerm> for OrderNone {
    type Output = OrderScalar;
}
impl ExtendOrderClass<ColumnTerm> for OrderScalar {
    type Output = OrderScalar;
}
impl ExtendOrderClass<ColumnTerm> for OrderAggregate {
    type Output = OrderMixed;
}
impl ExtendOrderClass<ColumnTerm> for OrderMixed {
    type Output = OrderMixed;
}
// An aggregate order term.
impl ExtendOrderClass<AggregateTerm> for OrderNone {
    type Output = OrderAggregate;
}
impl ExtendOrderClass<AggregateTerm> for OrderScalar {
    type Output = OrderMixed;
}
impl ExtendOrderClass<AggregateTerm> for OrderAggregate {
    type Output = OrderAggregate;
}
impl ExtendOrderClass<AggregateTerm> for OrderMixed {
    type Output = OrderMixed;
}

/// Witness that a select chain's order-class is valid for a projection of the given class: a scalar
/// projection may order only by scalar columns, an aggregate projection only by aggregates; either
/// may have no ordering. A mixed ordering is never valid.
#[doc(hidden)]
pub trait OrderCompatibleWith<ProjectionClass> {}
impl OrderCompatibleWith<ScalarProjection> for OrderNone {}
impl OrderCompatibleWith<ScalarProjection> for OrderScalar {}
impl OrderCompatibleWith<AggregateProjection> for OrderNone {}
impl OrderCompatibleWith<AggregateProjection> for OrderAggregate {}

// === GROUP BY state ===

/// Grouping state of a select chain (carried as [`SelectAst::Grouped`](crate::SelectAst::Grouped)):
/// the chain has no `GROUP BY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ungrouped {}
/// Grouping state of a select chain: the chain has a `GROUP BY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouped {}
/// Grouping state of a select chain: the chain has a `HAVING` but no `GROUP BY`, so SQL evaluates it
/// as a single (whole-table) group, and every `HAVING` predicate so far was column-free. The
/// projection and ordering must then be aggregate-only — a bare column is invalid without
/// `GROUP BY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aggregated {}
/// Grouping state of a select chain: a whole-table-aggregate `HAVING` (no `GROUP BY`) referenced a
/// bare column. That is only valid once a `GROUP BY` makes the column a grouping key, so this state
/// has no [`ValidSelect`] impl — `select` is rejected until a `group_by` rescues it to [`Grouped`]
/// (this keeps the check independent of whether `having` or `group_by` was called first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateNeedsGroupBy {}

/// Validates a `SELECT`'s projection and ordering for a chain's [grouping state](Ungrouped).
///
/// An [`Ungrouped`] query must be *homogeneous* — every projected element the same class
/// ([`ProjectionClass`]) — and its ordering must match ([`OrderCompatibleWith`]), since without
/// `GROUP BY` a list cannot mix a bare column with an aggregate. An [`Aggregated`] query (a `HAVING`
/// with no `GROUP BY`) is stricter still: the projection must be aggregate-*only*, since a bare
/// column has no group to belong to. [`AggregateNeedsGroupBy`] has no impl at all — it requires a
/// `GROUP BY` first. A [`Grouped`] query lifts those restrictions: a grouped list may mix grouping
/// keys and aggregates, and may order by either; the database validates that the non-aggregate terms
/// are grouping keys.
#[doc(hidden)]
pub trait ValidSelect<Projection, OrderClass> {}

impl<Projection, OrderClass> ValidSelect<Projection, OrderClass> for Grouped {}

impl<Projection, OrderClass> ValidSelect<Projection, OrderClass> for Ungrouped
where
    Projection: ProjectionClass,
    OrderClass: OrderCompatibleWith<<Projection as ProjectionClass>::Class>,
{
}

impl<Projection, OrderClass> ValidSelect<Projection, OrderClass> for Aggregated
where
    // Column-free projection (aggregates and constants), not necessarily *all* aggregate — a
    // constant like `SELECT 1` is valid in a whole-table aggregate. Ordering must still be
    // aggregate-only (no bare-column `ORDER BY`).
    Projection: ProjectionColumns<Columns = ColumnFree>,
    OrderClass: OrderCompatibleWith<AggregateProjection>,
{
}

/// Maps a chain's [grouping state](Ungrouped) through a `HAVING` whose predicate has column class
/// `PredicateColumns` ([`ColumnFree`] / [`HasBareColumn`]). A chain that already has a `GROUP BY`
/// stays [`Grouped`] for any predicate. Without one, a column-free predicate yields a whole-table
/// [`Aggregated`] chain, while a bare-column predicate yields [`AggregateNeedsGroupBy`] — selectable
/// only once a later `group_by` rescues it to [`Grouped`]. This makes the result independent of
/// whether `having` or `group_by` was written first.
#[doc(hidden)]
pub trait HavingTransition<PredicateColumns> {
    type Output;
}

impl<PredicateColumns> HavingTransition<PredicateColumns> for Grouped {
    type Output = Grouped;
}

impl HavingTransition<ColumnFree> for Ungrouped {
    type Output = Aggregated;
}

impl HavingTransition<HasBareColumn> for Ungrouped {
    type Output = AggregateNeedsGroupBy;
}

impl HavingTransition<ColumnFree> for Aggregated {
    type Output = Aggregated;
}

impl HavingTransition<HasBareColumn> for Aggregated {
    type Output = AggregateNeedsGroupBy;
}

impl<PredicateColumns> HavingTransition<PredicateColumns> for AggregateNeedsGroupBy {
    type Output = AggregateNeedsGroupBy;
}

/// Marker for predicate ASTs whose expression operands are all aggregate-free (see
/// [`NonAggregateAst`]). `where_` requires it, keeping aggregates out of `WHERE` clauses.
pub trait NonAggregatePredicate {}

impl<Left, Right> NonAggregatePredicate for ComparePredicateAst<Left, Right>
where
    Left: NonAggregateAst,
    Right: NonAggregateAst,
{
}

impl<Left, Right> NonAggregatePredicate for LikePredicateAst<Left, Right>
where
    Left: NonAggregateAst,
    Right: NonAggregateAst,
{
}

impl<Operand, V> NonAggregatePredicate for InPredicateAst<Operand, V> where Operand: NonAggregateAst {}

impl<Operand, Lo, Hi> NonAggregatePredicate for BetweenPredicateAst<Operand, Lo, Hi>
where
    Operand: NonAggregateAst,
    Lo: NonAggregateAst,
    Hi: NonAggregateAst,
{
}

impl<Operand> NonAggregatePredicate for NullCheckPredicateAst<Operand> where Operand: NonAggregateAst
{}

impl<Operand> NonAggregatePredicate for BoolTestPredicateAst<Operand> where Operand: NonAggregateAst {}

impl<Left, Right> NonAggregatePredicate for AndPredicateAst<Left, Right>
where
    Left: NonAggregatePredicate,
    Right: NonAggregatePredicate,
{
}

impl<Left, Right> NonAggregatePredicate for OrPredicateAst<Left, Right>
where
    Left: NonAggregatePredicate,
    Right: NonAggregatePredicate,
{
}

impl<P> NonAggregatePredicate for NotPredicateAst<P> where P: NonAggregatePredicate {}

/// Classifies a predicate AST as [`ColumnFree`] or [`HasBareColumn`] by combining its expression
/// operands' [`ExprColumns`] classes. Used to validate `HAVING`: a whole-table-aggregate `HAVING`
/// (no `GROUP BY`) requires a column-free predicate, mirroring how the projection must be
/// aggregate-only there, while a later `GROUP BY` makes a bare column valid (a grouping key).
#[doc(hidden)]
pub trait PredicateColumns {
    type Columns;
}

impl<Left, Right> PredicateColumns for ComparePredicateAst<Left, Right>
where
    Left: ExprColumns,
    Right: ExprColumns,
    <Left as ExprColumns>::Columns: CombineColumns<<Right as ExprColumns>::Columns>,
{
    type Columns =
        <<Left as ExprColumns>::Columns as CombineColumns<<Right as ExprColumns>::Columns>>::Output;
}

impl<Left, Right> PredicateColumns for LikePredicateAst<Left, Right>
where
    Left: ExprColumns,
    Right: ExprColumns,
    <Left as ExprColumns>::Columns: CombineColumns<<Right as ExprColumns>::Columns>,
{
    type Columns =
        <<Left as ExprColumns>::Columns as CombineColumns<<Right as ExprColumns>::Columns>>::Output;
}

impl<Operand, V> PredicateColumns for InPredicateAst<Operand, V>
where
    Operand: ExprColumns,
{
    type Columns = <Operand as ExprColumns>::Columns;
}

impl<Operand, Lo, Hi> PredicateColumns for BetweenPredicateAst<Operand, Lo, Hi>
where
    Operand: ExprColumns,
    Lo: ExprColumns,
    Hi: ExprColumns,
    <Operand as ExprColumns>::Columns: CombineColumns<<Lo as ExprColumns>::Columns>,
    <<Operand as ExprColumns>::Columns as CombineColumns<<Lo as ExprColumns>::Columns>>::Output:
        CombineColumns<<Hi as ExprColumns>::Columns>,
{
    type Columns = <<<Operand as ExprColumns>::Columns as CombineColumns<
        <Lo as ExprColumns>::Columns,
    >>::Output as CombineColumns<<Hi as ExprColumns>::Columns>>::Output;
}

impl<Operand> PredicateColumns for NullCheckPredicateAst<Operand>
where
    Operand: ExprColumns,
{
    type Columns = <Operand as ExprColumns>::Columns;
}

impl<Operand> PredicateColumns for BoolTestPredicateAst<Operand>
where
    Operand: ExprColumns,
{
    type Columns = <Operand as ExprColumns>::Columns;
}

impl<Left, Right> PredicateColumns for AndPredicateAst<Left, Right>
where
    Left: PredicateColumns,
    Right: PredicateColumns,
    <Left as PredicateColumns>::Columns: CombineColumns<<Right as PredicateColumns>::Columns>,
{
    type Columns = <<Left as PredicateColumns>::Columns as CombineColumns<
        <Right as PredicateColumns>::Columns,
    >>::Output;
}

impl<Left, Right> PredicateColumns for OrPredicateAst<Left, Right>
where
    Left: PredicateColumns,
    Right: PredicateColumns,
    <Left as PredicateColumns>::Columns: CombineColumns<<Right as PredicateColumns>::Columns>,
{
    type Columns = <<Left as PredicateColumns>::Columns as CombineColumns<
        <Right as PredicateColumns>::Columns,
    >>::Output;
}

impl<P> PredicateColumns for NotPredicateAst<P>
where
    P: PredicateColumns,
{
    type Columns = <P as PredicateColumns>::Columns;
}
// A subquery condition is its own scope: `IN (subquery)` exposes its outer operand's columns;
// `EXISTS (subquery)` references no outer bare column.
impl<Operand, Sub> PredicateColumns for InSubqueryPredicateAst<Operand, Sub>
where
    Operand: ExprColumns,
{
    type Columns = <Operand as ExprColumns>::Columns;
}
impl<Sub> PredicateColumns for ExistsPredicateAst<Sub> {
    type Columns = ColumnFree;
}

/// A predicate AST's [term](ConstantTerm) — its operand terms combined via [`CombineTerm`] (columns
/// preserved, *not* collapsed). Used to fold a `CASE` arm's `WHEN` condition into the result term: an
/// aggregate in the condition makes the `CASE` aggregate, and a *bare column* in the condition keeps
/// its column dependency (so `WHEN id > 0 THEN COUNT(..)` is rejected ungrouped, exactly as
/// `COUNT(id) + id` is). Mirrors [`PredicateColumns`] but over terms instead of columns.
#[doc(hidden)]
pub trait PredicateTerm {
    type Term;
}
impl<Left, Right> PredicateTerm for ComparePredicateAst<Left, Right>
where
    Left: AstProjectionClass,
    Right: AstProjectionClass,
    <Left as AstProjectionClass>::Class: CombineTerm<<Right as AstProjectionClass>::Class>,
{
    type Term = <<Left as AstProjectionClass>::Class as CombineTerm<
        <Right as AstProjectionClass>::Class,
    >>::Output;
}
impl<Left, Right> PredicateTerm for LikePredicateAst<Left, Right>
where
    Left: AstProjectionClass,
    Right: AstProjectionClass,
    <Left as AstProjectionClass>::Class: CombineTerm<<Right as AstProjectionClass>::Class>,
{
    type Term = <<Left as AstProjectionClass>::Class as CombineTerm<
        <Right as AstProjectionClass>::Class,
    >>::Output;
}
impl<Operand, V> PredicateTerm for InPredicateAst<Operand, V>
where
    Operand: AstProjectionClass,
{
    type Term = <Operand as AstProjectionClass>::Class;
}
impl<Operand, Lo, Hi> PredicateTerm for BetweenPredicateAst<Operand, Lo, Hi>
where
    Operand: AstProjectionClass,
    Lo: AstProjectionClass,
    Hi: AstProjectionClass,
    <Operand as AstProjectionClass>::Class: CombineTerm<<Lo as AstProjectionClass>::Class>,
    <<Operand as AstProjectionClass>::Class as CombineTerm<<Lo as AstProjectionClass>::Class>>::Output:
        CombineTerm<<Hi as AstProjectionClass>::Class>,
{
    type Term = <<<Operand as AstProjectionClass>::Class as CombineTerm<
        <Lo as AstProjectionClass>::Class,
    >>::Output as CombineTerm<<Hi as AstProjectionClass>::Class>>::Output;
}
impl<Operand> PredicateTerm for NullCheckPredicateAst<Operand>
where
    Operand: AstProjectionClass,
{
    type Term = <Operand as AstProjectionClass>::Class;
}
impl<Operand> PredicateTerm for BoolTestPredicateAst<Operand>
where
    Operand: AstProjectionClass,
{
    type Term = <Operand as AstProjectionClass>::Class;
}
impl<Left, Right> PredicateTerm for AndPredicateAst<Left, Right>
where
    Left: PredicateTerm,
    Right: PredicateTerm,
    <Left as PredicateTerm>::Term: CombineTerm<<Right as PredicateTerm>::Term>,
{
    type Term =
        <<Left as PredicateTerm>::Term as CombineTerm<<Right as PredicateTerm>::Term>>::Output;
}
impl<Left, Right> PredicateTerm for OrPredicateAst<Left, Right>
where
    Left: PredicateTerm,
    Right: PredicateTerm,
    <Left as PredicateTerm>::Term: CombineTerm<<Right as PredicateTerm>::Term>,
{
    type Term =
        <<Left as PredicateTerm>::Term as CombineTerm<<Right as PredicateTerm>::Term>>::Output;
}
impl<P> PredicateTerm for NotPredicateAst<P>
where
    P: PredicateTerm,
{
    type Term = <P as PredicateTerm>::Term;
}
// A subquery condition is its own scope: `IN (subquery)` contributes its outer operand's term (so an
// outer bare column or aggregate is still accounted for), and `EXISTS (subquery)` has no outer operand.
impl<Operand, Sub> PredicateTerm for InSubqueryPredicateAst<Operand, Sub>
where
    Operand: AstProjectionClass,
{
    type Term = <Operand as AstProjectionClass>::Class;
}
impl<Sub> PredicateTerm for ExistsPredicateAst<Sub> {
    type Term = ConstantTerm;
}

#[doc(hidden)]
pub trait ExprVisitor {
    type Error;

    /// The backend this visitor renders for. Literal encoding is resolved against it.
    type Backend: crate::Backend;

    fn visit_column(&mut self, alias: SourceAlias, column: &str) -> Result<(), Self::Error>;

    fn visit_literal<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: crate::Encode<Self::Backend>;

    fn visit_param(&mut self) -> Result<(), Self::Error>;

    fn visit_binary<L, R>(
        &mut self,
        op: ArithmeticOp,
        left: L,
        right: R,
    ) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>;

    /// Render a SQL aggregate function call (`func(operand)`), optionally wrapped in a
    /// `CAST(... AS cast)` so the result type matches the advertised Rust type.
    fn visit_aggregate<O>(
        &mut self,
        func: AggregateFunc,
        distinct: bool,
        cast: Option<&crate::SqlType>,
        operand: O,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>;

    /// Render a scalar subquery — a single-row, single-column `(SELECT …)` used as a value
    /// expression. The subquery shares the parent's placeholder numbering.
    fn visit_scalar_subquery<Sub>(&mut self, subquery: &Sub) -> Result<(), Self::Error>
    where
        Sub: crate::RenderSubquery<Self::Backend>;

    /// Render a window function: `func(operand) OVER (PARTITION BY … ORDER BY …)`, optionally wrapped
    /// in `CAST(… AS cast)`. `operand` renders the function arguments (nothing for `ROW_NUMBER()`);
    /// `partitions`/`orders` render their lists (each uses [`visit_window_separator`] between
    /// elements, and orders use [`visit_window_order_direction`]). The `has_*` flags say whether to
    /// emit the `PARTITION BY` / `ORDER BY` keyword.
    #[allow(clippy::too_many_arguments)]
    fn visit_window<Operand, Partitions, Orders>(
        &mut self,
        func: WindowFunc,
        cast: Option<&crate::SqlType>,
        operand: Operand,
        has_partitions: bool,
        partitions: Partitions,
        has_orders: bool,
        orders: Orders,
    ) -> Result<(), Self::Error>
    where
        Operand: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Partitions: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Orders: FnOnce(&mut Self) -> Result<(), Self::Error>;

    /// Render the separator (`, `) between elements of a window `PARTITION BY` / `ORDER BY` list.
    fn visit_window_separator(&mut self) -> Result<(), Self::Error>;

    /// Render a window `ORDER BY` term's direction (` ASC` / ` DESC`).
    fn visit_window_order_direction(
        &mut self,
        direction: OrderDirection,
    ) -> Result<(), Self::Error>;

    /// Render a searched `CASE WHEN … THEN … [ELSE …] END`. `arms` renders each `WHEN`/`THEN` pair
    /// (emitting [`visit_case_when`](Self::visit_case_when) / [`visit_case_then`](Self::visit_case_then)
    /// around the predicate and value); `else_` is the optional `ELSE` value. The implementor emits the
    /// `CASE` / ` ELSE ` / ` END` keywords.
    fn visit_case<Arms, Else>(
        &mut self,
        arms: &Arms,
        else_: Option<&Else>,
        result: Option<&crate::SqlType>,
    ) -> Result<(), Self::Error>
    where
        Arms: RenderCaseArms<Self::Backend>,
        Else: RenderAst<Self::Backend>;

    /// Emit the ` WHEN ` keyword before a `CASE` arm's predicate.
    fn visit_case_when(&mut self) -> Result<(), Self::Error>;

    /// Emit the ` THEN ` keyword between a `CASE` arm's predicate and value.
    fn visit_case_then(&mut self) -> Result<(), Self::Error>;

    /// Open a `CASE` branch value: emit `CAST(` when `cast` is set (so an all-parameter branch is
    /// typeable), nothing otherwise. Paired with [`visit_case_value_close`](Self::visit_case_value_close)
    /// around each `THEN`/`ELSE` value.
    fn visit_case_value_open(&mut self, cast: Option<&crate::SqlType>) -> Result<(), Self::Error>;

    /// Close a `CASE` branch value: emit ` AS <cast>)` when `cast` is set, nothing otherwise.
    fn visit_case_value_close(&mut self, cast: Option<&crate::SqlType>) -> Result<(), Self::Error>;
}

#[doc(hidden)]
pub trait PredicateAstVisitor: ExprVisitor {
    fn visit_compare<L, R>(&mut self, op: CompareOp, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>;

    fn visit_and<L, R>(&mut self, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>;

    fn visit_or<L, R>(&mut self, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>;

    fn visit_not<P>(&mut self, predicate: P) -> Result<(), Self::Error>
    where
        P: FnOnce(&mut Self) -> Result<(), Self::Error>;

    /// Render a SQL `IS NULL` (or `IS NOT NULL` when `negated`) test of `operand`.
    fn visit_is_null<O>(&mut self, negated: bool, operand: O) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>;

    /// Render a SQL `LIKE` pattern match. `case_insensitive` selects `ILIKE` on dialects that
    /// support it (and a case-insensitive fallback elsewhere); `negated` selects `NOT LIKE`.
    fn visit_like<O, P>(
        &mut self,
        case_insensitive: bool,
        negated: bool,
        operand: O,
        pattern: P,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        P: FnOnce(&mut Self) -> Result<(), Self::Error>;

    /// Render a SQL `IN` (or `NOT IN` when `negated`) test of `operand` against a literal value
    /// list. An empty list is rendered as a constant `FALSE` (or `TRUE` when negated) since SQL has
    /// no `IN ()` form.
    fn visit_in<O, T>(
        &mut self,
        negated: bool,
        operand: O,
        values: &[T],
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        T: crate::Encode<Self::Backend>;

    /// Render a SQL `BETWEEN lo AND hi` (or `NOT BETWEEN` when `negated`) range test.
    fn visit_between<O, Lo, Hi>(
        &mut self,
        negated: bool,
        operand: O,
        lo: Lo,
        hi: Hi,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Lo: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Hi: FnOnce(&mut Self) -> Result<(), Self::Error>;

    /// Render a boolean-valued expression used directly as a predicate (`negated` wraps it in
    /// `NOT`).
    fn visit_bool_test<O>(&mut self, negated: bool, operand: O) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>;

    /// Render a SQL `operand IN (subquery)` (or `NOT IN` when `negated`) membership test. The
    /// subquery renders as a nested `(SELECT …)`, sharing the parent's placeholder numbering.
    fn visit_in_subquery<O, Sub>(
        &mut self,
        negated: bool,
        operand: O,
        subquery: &Sub,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Sub: crate::RenderSubquery<Self::Backend>;

    /// Render a SQL `EXISTS (subquery)` (or `NOT EXISTS` when `negated`) test.
    fn visit_exists<Sub>(&mut self, negated: bool, subquery: &Sub) -> Result<(), Self::Error>
    where
        Sub: crate::RenderSubquery<Self::Backend>;
}

macro_rules! impl_value_expr_kind {
    ($($ty:ty),* $(,)?) => {
        $(impl ExprKind for $ty {
            type Value = $ty;
        }
        impl IntoWindowNullable for $ty {
            type Kind = ScalarNullable<$ty>;
        })*
    };
}

impl_value_expr_kind!(i8, i16, i32, i64, i128, isize);
impl_value_expr_kind!(u8, u16, u32, u64, u128, usize);
impl_value_expr_kind!(f32, f64);
impl_value_expr_kind!(String, bool);
// `Vec<u8>` is a `bytea`/`BLOB` column value type (binary payloads), usable as a literal predicate
// operand and a write-builder setter like the scalar value types above.
impl_value_expr_kind!(Vec<u8>);

/// Maps a window operand's kind to its `LAG`/`LEAD` result kind, which is always nullable (`NULL`
/// past the partition edge). The mapping is idempotent over nullability: an already-nullable
/// left-join projection (`Nullable<K>`) stays `Nullable<K>` so the result decodes as a single
/// `Option<T>` rather than `Option<Option<T>>`, while a non-null kind becomes `ScalarNullable<K>`.
/// Implemented for value types (here), nullable kinds (below), and each column kind (via the `Table`
/// derive).
#[doc(hidden)]
pub trait IntoWindowNullable {
    type Kind: ExprKind;
}
impl<K> IntoWindowNullable for Nullable<K>
where
    K: ExprKind,
{
    type Kind = Nullable<K>;
}
impl<K> IntoWindowNullable for ScalarNullable<K>
where
    K: ExprKind,
{
    type Kind = ScalarNullable<K>;
}

// Derived scalar expression kinds (arithmetic, runtime params) are non-null, so a LAG/LEAD over
// them becomes `ScalarNullable<…>` — letting `lag(post.id + 1, 1)` and friends compile.
macro_rules! impl_into_window_nullable_for_expr_kind {
    ($($ty:ty),* $(,)?) => {
        $(impl<L, R> IntoWindowNullable for $ty
        where
            $ty: ExprKind,
        {
            type Kind = ScalarNullable<$ty>;
        })*
    };
}
impl_into_window_nullable_for_expr_kind!(
    AddExpr<L, R>,
    SubtractExpr<L, R>,
    MultiplyExpr<L, R>,
    DivideExpr<L, R>,
);
impl<K> IntoWindowNullable for RuntimeParam<K>
where
    RuntimeParam<K>: ExprKind,
{
    type Kind = ScalarNullable<RuntimeParam<K>>;
}

/// A `uuid::Uuid` value can be used as a literal predicate operand (`col.equals(id)`) or a
/// write-builder setter (`.id(id)`), like the scalar value types above.
#[cfg(feature = "uuid")]
impl ExprKind for uuid::Uuid {
    type Value = uuid::Uuid;
}

// Native timestamp values can be used as literal predicate operands and write-builder setters.
#[cfg(feature = "systemtime")]
impl_value_expr_kind!(std::time::SystemTime);
#[cfg(feature = "time")]
impl_value_expr_kind!(time::OffsetDateTime);
#[cfg(feature = "chrono")]
impl_value_expr_kind!(chrono::DateTime<chrono::Utc>);

/// Type-level identity for a nullable SQL expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Nullable<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for Nullable<K>
where
    K: ExprKind,
{
    type Value = Option<K::Value>;
}

/// Marker for expression kinds that may be SQL `NULL`. It gates the `is_null` / `is_not_null`
/// builders so they are only callable on nullable operands, making an `IS NULL` test of a column
/// the type system knows is `NOT NULL` a compile error.
///
/// Implemented for [`Nullable<K>`] (outer-join projections and explicitly nullable expressions) and,
/// by the `Table` derive, for the column kind of every `#[column(nullable)]` field.
pub trait NullableExpr {}

impl<K> NullableExpr for Nullable<K> {}

/// Type-level kind for a scalar subquery result. Its value type stays `K::Value` — so it compares
/// against the same operands as `K` — but it is always nullable, because a scalar subquery that
/// matches zero rows evaluates to SQL `NULL` regardless of the projected column's own nullability.
/// It therefore decodes as `Option<K::Value>` and supports [`is_null`](Expr::is_null). (Unlike
/// [`Nullable<K>`], whose *value* is `Option<…>`, this keeps the bare value type for comparison while
/// only the decoded/projected row type gains the `Option`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScalarNullable<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for ScalarNullable<K>
where
    K: ExprKind,
{
    type Value = K::Value;
}

impl<K> NullableExpr for ScalarNullable<K> {}

/// The runtime-parameter shape contributed by a projection's own expressions. An embedded subquery
/// renders its `SELECT` list *before* its `FROM`/`WHERE`/…, so a runtime [`param`] appearing in the
/// projection must be counted ahead of the rest of the subquery's params (see
/// [`Subquery::Params`](crate::Subquery::Params)); otherwise it would be silently dropped from the
/// surrounding query's bind list. Implemented for the single-column projection forms an embeddable
/// subquery uses: a bare column or value carries no params, an expression carries its AST's.
pub trait ProjectionParams {
    type Params: crate::HList;
}

impl<'scope, K, Ast> ProjectionParams for Expr<'scope, K, Ast>
where
    Ast: ExprAst,
{
    type Params = Ast::Params;
}

impl<K> ProjectionParams for ColumnRef<'_, K> {
    type Params = crate::HNil;
}

impl<T> ProjectionParams for T
where
    T: ExprKind<Value = T>,
{
    type Params = crate::HNil;
}

impl ProjectionParams for () {
    type Params = crate::HNil;
}

/// Type-level identity for a prepared statement runtime parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeParam<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for RuntimeParam<K>
where
    K: ExprKind,
{
    type Value = K::Value;
}

/// Type-level identity for SQL addition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddExpr<L, R> {
    _Marker(PhantomData<(L, R)>),
}

impl<L, R> ExprKind for AddExpr<L, R>
where
    L: ExprKind,
    R: ExprKind<Value = L::Value>,
    L::Value: SqlNumber,
{
    type Value = L::Value;
}

/// Type-level identity for SQL subtraction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubtractExpr<L, R> {
    _Marker(PhantomData<(L, R)>),
}

impl<L, R> ExprKind for SubtractExpr<L, R>
where
    L: ExprKind,
    R: ExprKind<Value = L::Value>,
    L::Value: SqlNumber,
{
    type Value = L::Value;
}

/// Type-level identity for SQL multiplication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiplyExpr<L, R> {
    _Marker(PhantomData<(L, R)>),
}

impl<L, R> ExprKind for MultiplyExpr<L, R>
where
    L: ExprKind,
    R: ExprKind<Value = L::Value>,
    L::Value: SqlNumber,
{
    type Value = L::Value;
}

/// Type-level identity for SQL division.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DivideExpr<L, R> {
    _Marker(PhantomData<(L, R)>),
}

impl<L, R> ExprKind for DivideExpr<L, R>
where
    L: ExprKind,
    R: ExprKind<Value = L::Value>,
    L::Value: SqlDivide,
{
    type Value = <L::Value as SqlDivide>::Output;
}

/// Type-level identity for SQL `COUNT(expr)`. `COUNT` never returns `NULL` (it is `0` for an empty
/// input), so its value type is a non-null `i64`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CountExpr<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for CountExpr<K>
where
    K: ExprKind,
{
    type Value = i64;
}

/// Type-level identity for SQL `SUM(expr)`. A sum is `NULL` over an empty input, so the value type
/// is nullable; the operand type is widened per [`SqlSum`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SumExpr<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for SumExpr<K>
where
    K: ExprKind,
    K::Value: AggregateScalar,
    <K::Value as AggregateScalar>::Scalar: SqlSum,
{
    type Value = Option<<<K::Value as AggregateScalar>::Scalar as SqlSum>::Output>;
}

/// Type-level identity for SQL `AVG(expr)`. An average is `NULL` over an empty input and always
/// fractional, so the value type is `Option<f64>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AvgExpr<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for AvgExpr<K>
where
    K: ExprKind,
    K::Value: AggregateScalar,
    <K::Value as AggregateScalar>::Scalar: SqlNumber,
{
    type Value = Option<f64>;
}

/// Type-level identity for SQL `MIN(expr)`. `MIN` is `NULL` over an empty input, so the value type
/// is nullable in the operand's own type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MinExpr<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for MinExpr<K>
where
    K: ExprKind,
    K::Value: AggregateScalar,
{
    type Value = Option<<K::Value as AggregateScalar>::Scalar>;
}

/// Type-level identity for SQL `MAX(expr)`. `MAX` is `NULL` over an empty input, so the value type
/// is nullable in the operand's own type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaxExpr<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for MaxExpr<K>
where
    K: ExprKind,
    K::Value: AggregateScalar,
{
    type Value = Option<<K::Value as AggregateScalar>::Scalar>;
}

/// Type-level identity for a SQL predicate.
pub trait PredicateKind {}

macro_rules! predicate_kind {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Clone, Debug, PartialEq, Eq)]
            pub enum $name<L, R> {
                _Marker(PhantomData<(L, R)>),
            }

            impl<L, R> PredicateKind for $name<L, R> {}
        )*
    };
}

/// Type-level identity for predicates whose exact shape has been erased.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnyPredicate {}

impl PredicateKind for AnyPredicate {}

predicate_kind!(
    EqualsPredicate,
    NotEqualsPredicate,
    LessThanPredicate,
    LessThanOrEqualsPredicate,
    GreaterThanPredicate,
    GreaterThanOrEqualsPredicate,
    AndPredicate,
    OrPredicate,
    LikePredicate,
);

/// Type-level identity for a SQL `IN (...)` list membership test of an expression of kind `K`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InPredicate<K> {
    _Marker(PhantomData<K>),
}

impl<K> PredicateKind for InPredicate<K> {}

/// Type-level identity for a SQL `EXISTS (subquery)` test (not tied to any column kind).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExistsPredicate {}

impl PredicateKind for ExistsPredicate {}

/// Type-level identity for a SQL `BETWEEN` range test of an operand of kind `K`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BetweenPredicate<K> {
    _Marker(PhantomData<K>),
}

impl<K> PredicateKind for BetweenPredicate<K> {}

/// Type-level identity for a boolean-valued expression of kind `K` used directly as a predicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoolTestPredicate<K> {
    _Marker(PhantomData<K>),
}

impl<K> PredicateKind for BoolTestPredicate<K> {}

/// Type-level identity for SQL predicate negation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotPredicate<P> {
    _Marker(PhantomData<P>),
}

impl<P> PredicateKind for NotPredicate<P> {}

/// Type-level identity for a SQL `IS NULL` test of an expression of kind `K`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IsNullPredicate<K> {
    _Marker(PhantomData<K>),
}

impl<K> PredicateKind for IsNullPredicate<K> {}

/// Type-level identity for a SQL `IS NOT NULL` test of an expression of kind `K`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IsNotNullPredicate<K> {
    _Marker(PhantomData<K>),
}

impl<K> PredicateKind for IsNotNullPredicate<K> {}

#[doc(hidden)]
pub trait PredicateAst: Clone {
    type Params: crate::HList;
}

/// Backend-parameterized rendering for a predicate AST node (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderPredicateAst<B>: PredicateAst
where
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>;
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct ComparePredicateAst<Left, Right> {
    left: Left,
    op: CompareOp,
    right: Right,
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct AndPredicateAst<Left, Right> {
    left: Left,
    right: Right,
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct OrPredicateAst<Left, Right> {
    left: Left,
    right: Right,
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct NotPredicateAst<Predicate> {
    predicate: Predicate,
}

/// Unary `IS NULL` / `IS NOT NULL` test of a single expression operand. `negated` selects
/// `IS NOT NULL`; the operand's parameters flow straight through (a column contributes none).
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct NullCheckPredicateAst<Operand> {
    operand: Operand,
    negated: bool,
}

impl<Left, Right> PredicateAst for ComparePredicateAst<Left, Right>
where
    Left: ExprAst,
    Right: ExprAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;
}

impl<Left, Right, B> RenderPredicateAst<B> for ComparePredicateAst<Left, Right>
where
    Left: RenderAst<B>,
    Right: RenderAst<B>,
    Left::Params: crate::HAppend<Right::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_compare(
            self.op,
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

impl<Left, Right> PredicateAst for AndPredicateAst<Left, Right>
where
    Left: PredicateAst,
    Right: PredicateAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;
}

impl<Left, Right, B> RenderPredicateAst<B> for AndPredicateAst<Left, Right>
where
    Left: RenderPredicateAst<B>,
    Right: RenderPredicateAst<B>,
    Left::Params: crate::HAppend<Right::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_and(
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

impl<Left, Right> PredicateAst for OrPredicateAst<Left, Right>
where
    Left: PredicateAst,
    Right: PredicateAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;
}

impl<Left, Right, B> RenderPredicateAst<B> for OrPredicateAst<Left, Right>
where
    Left: RenderPredicateAst<B>,
    Right: RenderPredicateAst<B>,
    Left::Params: crate::HAppend<Right::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_or(
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

impl<Predicate> PredicateAst for NotPredicateAst<Predicate>
where
    Predicate: PredicateAst,
{
    type Params = Predicate::Params;
}

impl<Predicate, B> RenderPredicateAst<B> for NotPredicateAst<Predicate>
where
    Predicate: RenderPredicateAst<B>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_not(|visitor| self.predicate.visit(visitor))
    }
}

impl<Operand> PredicateAst for NullCheckPredicateAst<Operand>
where
    Operand: ExprAst,
{
    type Params = Operand::Params;
}

impl<Operand, B> RenderPredicateAst<B> for NullCheckPredicateAst<Operand>
where
    Operand: RenderAst<B>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_is_null(self.negated, |visitor| self.operand.visit(visitor))
    }
}

/// `LIKE` / `ILIKE` pattern match between two expression operands. `case_insensitive` selects
/// `ILIKE`; `negated` selects the `NOT` form. The operands' parameters concatenate left-to-right,
/// like a comparison.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct LikePredicateAst<Left, Right> {
    left: Left,
    right: Right,
    case_insensitive: bool,
    negated: bool,
}

impl<Left, Right> PredicateAst for LikePredicateAst<Left, Right>
where
    Left: ExprAst,
    Right: ExprAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;
}

impl<Left, Right, B> RenderPredicateAst<B> for LikePredicateAst<Left, Right>
where
    Left: RenderAst<B>,
    Right: RenderAst<B>,
    Left::Params: crate::HAppend<Right::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_like(
            self.case_insensitive,
            self.negated,
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

/// `IN (...)` / `NOT IN (...)` membership test of an operand against an inline list of value
/// literals. The values are captured in the AST and encoded as binds at render time, so they
/// contribute no runtime parameters — only the operand's parameters flow through.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct InPredicateAst<Operand, V> {
    operand: Operand,
    values: Vec<V>,
    negated: bool,
}

impl<Operand, V> PredicateAst for InPredicateAst<Operand, V>
where
    Operand: ExprAst,
    V: Clone,
{
    type Params = Operand::Params;
}

impl<Operand, V, B> RenderPredicateAst<B> for InPredicateAst<Operand, V>
where
    Operand: RenderAst<B>,
    V: Clone + crate::Encode<B>,
    B: crate::Backend,
{
    fn visit<Vi>(&self, visitor: &mut Vi) -> Result<(), Vi::Error>
    where
        Vi: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_in(
            self.negated,
            |visitor| self.operand.visit(visitor),
            &self.values,
        )
    }
}

/// `operand IN (subquery)` / `NOT IN (subquery)` membership against a single-column subquery. The
/// operand's parameters come first, then the subquery's, matching render order.
#[doc(hidden)]
#[derive(Clone)]
pub struct InSubqueryPredicateAst<Operand, Sub> {
    operand: Operand,
    subquery: Sub,
    negated: bool,
}

impl<Operand, Sub> PredicateAst for InSubqueryPredicateAst<Operand, Sub>
where
    Operand: ExprAst,
    Sub: crate::Subquery,
    Operand::Params: crate::HAppend<Sub::Params>,
{
    type Params = <Operand::Params as crate::HAppend<Sub::Params>>::Output;
}

impl<Operand, Sub, B> RenderPredicateAst<B> for InSubqueryPredicateAst<Operand, Sub>
where
    Operand: RenderAst<B>,
    Sub: crate::RenderSubquery<B>,
    Operand::Params: crate::HAppend<Sub::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_in_subquery(
            self.negated,
            |visitor| self.operand.visit(visitor),
            &self.subquery,
        )
    }
}

impl<Operand, Sub> NonAggregatePredicate for InSubqueryPredicateAst<Operand, Sub> where
    Operand: NonAggregateAst
{
}

/// `EXISTS (subquery)` / `NOT EXISTS (subquery)`. Only the subquery contributes parameters.
#[doc(hidden)]
#[derive(Clone)]
pub struct ExistsPredicateAst<Sub> {
    subquery: Sub,
    negated: bool,
}

impl<Sub> PredicateAst for ExistsPredicateAst<Sub>
where
    Sub: crate::Subquery,
{
    type Params = Sub::Params;
}

impl<Sub, B> RenderPredicateAst<B> for ExistsPredicateAst<Sub>
where
    Sub: crate::RenderSubquery<B>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_exists(self.negated, &self.subquery)
    }
}

impl<Sub> NonAggregatePredicate for ExistsPredicateAst<Sub> {}

/// SQL `EXISTS (subquery)` predicate. The subquery is typically correlated to the outer query.
/// Build the subquery with the [`Subqueries`](crate::Subqueries) handle from
/// [`where_correlated`](crate::SourceQuery::where_correlated).
pub fn exists<'scope, Sub>(
    subquery: Sub,
) -> Predicate<'scope, ExistsPredicate, ExistsPredicateAst<Sub>>
where
    Sub: crate::Subquery,
{
    Predicate::new(ExistsPredicateAst {
        subquery,
        negated: false,
    })
}

/// SQL `NOT EXISTS (subquery)`; see [`exists`].
pub fn not_exists<'scope, Sub>(
    subquery: Sub,
) -> Predicate<'scope, ExistsPredicate, ExistsPredicateAst<Sub>>
where
    Sub: crate::Subquery,
{
    Predicate::new(ExistsPredicateAst {
        subquery,
        negated: true,
    })
}

/// A scalar subquery used as a value expression: a single-row, single-column `(SELECT …)`.
#[doc(hidden)]
#[derive(Clone)]
pub struct ScalarSubqueryExprAst<Sub> {
    subquery: Sub,
}

impl<Sub> ExprAst for ScalarSubqueryExprAst<Sub>
where
    Sub: crate::Subquery,
{
    type Params = Sub::Params;
}

impl<Sub, B> RenderAst<B> for ScalarSubqueryExprAst<Sub>
where
    Sub: crate::RenderSubquery<B>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_scalar_subquery(&self.subquery)
    }
}

// A scalar subquery is a value, not an aggregate of the surrounding row, so it may appear in a
// `GROUP BY` key or a `WHERE` predicate.
impl<Sub> NonAggregateAst for ScalarSubqueryExprAst<Sub> {}

// In a projection a scalar subquery behaves like a (data-dependent) column: it makes the projection
// a `ScalarProjection`, and — like a bare column — may not be mixed with an aggregate absent a
// `GROUP BY`.
impl<Sub> AstProjectionClass for ScalarSubqueryExprAst<Sub> {
    type Class = ColumnTerm;
}

/// Build a scalar subquery expression: a single-row, single-column `(SELECT …)` usable anywhere an
/// [`Expr`] is — in a projection or as a comparison operand. The subquery may be correlated.
///
/// The result keeps the projected column's value type (so a `ColumnType` newtype is preserved and
/// `x.equals(scalar_subquery(..))` type-checks against the same operands), but is **always nullable**
/// ([`ScalarNullable`]): a scalar subquery that matches zero rows is SQL `NULL` even when the
/// selected column is non-null, so it decodes as `Option<T>` and can be tested with
/// [`is_null`](Expr::is_null). Returning more than one row at runtime is a SQL error, as in
/// hand-written SQL.
pub fn scalar_subquery<'scope, Sub>(
    subquery: Sub,
) -> Expr<'scope, ScalarNullable<Sub::OutputKind>, ScalarSubqueryExprAst<Sub>>
where
    Sub: crate::ScalarSubquery,
{
    Expr {
        ast: ScalarSubqueryExprAst { subquery },
        project_alias: Cow::Borrowed("expr"),
        _phantom: PhantomData,
    }
}

/// `BETWEEN lo AND hi` / `NOT BETWEEN` range test. The operand's, `lo`'s, and `hi`'s parameters
/// concatenate left-to-right.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct BetweenPredicateAst<Operand, Lo, Hi> {
    operand: Operand,
    lo: Lo,
    hi: Hi,
    negated: bool,
}

impl<Operand, Lo, Hi> PredicateAst for BetweenPredicateAst<Operand, Lo, Hi>
where
    Operand: ExprAst,
    Lo: ExprAst,
    Hi: ExprAst,
    Operand::Params: crate::HAppend<Lo::Params>,
    <Operand::Params as crate::HAppend<Lo::Params>>::Output: crate::HAppend<Hi::Params>,
{
    type Params = <<Operand::Params as crate::HAppend<Lo::Params>>::Output as crate::HAppend<
        Hi::Params,
    >>::Output;
}

impl<Operand, Lo, Hi, B> RenderPredicateAst<B> for BetweenPredicateAst<Operand, Lo, Hi>
where
    Operand: RenderAst<B>,
    Lo: RenderAst<B>,
    Hi: RenderAst<B>,
    Operand::Params: crate::HAppend<Lo::Params>,
    <Operand::Params as crate::HAppend<Lo::Params>>::Output: crate::HAppend<Hi::Params>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_between(
            self.negated,
            |visitor| self.operand.visit(visitor),
            |visitor| self.lo.visit(visitor),
            |visitor| self.hi.visit(visitor),
        )
    }
}

/// A boolean-valued expression used directly as a predicate (`negated` wraps it in `NOT`). The
/// operand's parameters flow straight through.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct BoolTestPredicateAst<Operand> {
    operand: Operand,
    negated: bool,
}

impl<Operand> PredicateAst for BoolTestPredicateAst<Operand>
where
    Operand: ExprAst,
{
    type Params = Operand::Params;
}

impl<Operand, B> RenderPredicateAst<B> for BoolTestPredicateAst<Operand>
where
    Operand: RenderAst<B>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor<Backend = B>,
    {
        visitor.visit_bool_test(self.negated, |visitor| self.operand.visit(visitor))
    }
}

/// Renders an expression operand to a self-contained ANSI SQL fragment for use inside a DDL
/// partial-index predicate (`CREATE UNIQUE INDEX ... WHERE <predicate>`).
///
/// Unlike [`RenderAst`], this path is **backend-independent** and carries no bind parameters:
/// columns render as bare, double-quoted identifiers with no source alias, and a value literal
/// renders *inline* (no `$n` placeholder) via [`DdlSqlLiteral`]. It is implemented for columns and
/// for literals of the scalar value types; operands whose SQL form is backend-specific (a runtime
/// bind param, an arithmetic sub-expression, or a literal of a temporal/uuid type) do not satisfy
/// the bound and fail to compile.
#[doc(hidden)]
pub trait DdlExprAst {
    fn render_ddl(&self, out: &mut String);
}

/// Renders a predicate node to a self-contained ANSI SQL string for a DDL partial-index
/// predicate. See [`DdlExprAst`] for the backend-independent contract.
#[doc(hidden)]
pub trait DdlPredicateAst {
    fn render_ddl(&self, out: &mut String);
}

/// A Rust value that renders as an inline ANSI SQL literal inside a DDL partial-index predicate
/// (e.g. `... WHERE "status" = 0`). Implemented for the scalar value types; types whose SQL
/// literal form is backend-specific or ambiguous (timestamps, `uuid`) are intentionally omitted, so
/// comparing such a column to a literal in a partial-index predicate is a compile error rather than
/// producing dialect-dependent or malformed SQL.
#[doc(hidden)]
pub trait DdlSqlLiteral {
    fn render_sql_literal(&self, out: &mut String);
}

macro_rules! impl_ddl_sql_literal_display {
    ($($ty:ty),* $(,)?) => {
        $(impl DdlSqlLiteral for $ty {
            fn render_sql_literal(&self, out: &mut String) {
                use ::std::fmt::Write as _;
                // Integer/float `Display` is already a valid SQL numeric literal.
                let _ = write!(out, "{self}");
            }
        })*
    };
}

impl_ddl_sql_literal_display!(i8, i16, i32, i64, i128, isize);
impl_ddl_sql_literal_display!(u8, u16, u32, u64, u128, usize);
impl_ddl_sql_literal_display!(f32, f64);

impl DdlSqlLiteral for bool {
    fn render_sql_literal(&self, out: &mut String) {
        out.push_str(if *self { "TRUE" } else { "FALSE" });
    }
}

impl DdlSqlLiteral for String {
    fn render_sql_literal(&self, out: &mut String) {
        // Standard SQL single-quoted string with embedded quotes doubled, matching the backend
        // text-literal quoting. Values originate from compile-time predicates, so this is
        // correctness (a slug like `o'brien`), not injection defense.
        out.push('\'');
        for ch in self.chars() {
            if ch == '\'' {
                out.push('\'');
            }
            out.push(ch);
        }
        out.push('\'');
    }
}

impl<K> DdlExprAst for ColumnExprAst<K> {
    fn render_ddl(&self, out: &mut String) {
        out.push('"');
        for ch in self.column.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    }
}

impl<K> DdlExprAst for LiteralExprAst<K>
where
    K: ExprKind,
    K::Value: DdlSqlLiteral,
{
    fn render_ddl(&self, out: &mut String) {
        self.value.render_sql_literal(out);
    }
}

impl<Operand> DdlPredicateAst for NullCheckPredicateAst<Operand>
where
    Operand: DdlExprAst,
{
    fn render_ddl(&self, out: &mut String) {
        out.push('(');
        self.operand.render_ddl(out);
        out.push_str(if self.negated {
            " IS NOT NULL)"
        } else {
            " IS NULL)"
        });
    }
}

impl<Left, Right> DdlPredicateAst for ComparePredicateAst<Left, Right>
where
    Left: DdlExprAst,
    Right: DdlExprAst,
{
    fn render_ddl(&self, out: &mut String) {
        out.push('(');
        self.left.render_ddl(out);
        out.push(' ');
        out.push_str(crate::render::render_compare_op(self.op));
        out.push(' ');
        self.right.render_ddl(out);
        out.push(')');
    }
}

impl<Left, Right> DdlPredicateAst for AndPredicateAst<Left, Right>
where
    Left: DdlPredicateAst,
    Right: DdlPredicateAst,
{
    fn render_ddl(&self, out: &mut String) {
        out.push('(');
        self.left.render_ddl(out);
        out.push_str(" AND ");
        self.right.render_ddl(out);
        out.push(')');
    }
}

impl<Left, Right> DdlPredicateAst for OrPredicateAst<Left, Right>
where
    Left: DdlPredicateAst,
    Right: DdlPredicateAst,
{
    fn render_ddl(&self, out: &mut String) {
        out.push('(');
        self.left.render_ddl(out);
        out.push_str(" OR ");
        self.right.render_ddl(out);
        out.push(')');
    }
}

impl<Predicate> DdlPredicateAst for NotPredicateAst<Predicate>
where
    Predicate: DdlPredicateAst,
{
    fn render_ddl(&self, out: &mut String) {
        out.push_str("(NOT ");
        self.predicate.render_ddl(out);
        out.push(')');
    }
}

/// Render a typed, literal-free [`Predicate`] to a self-contained ANSI SQL string suitable for a
/// DDL partial-index `WHERE` clause. Used by the `Table` derive to lower a `where = |row| ...`
/// attribute on a unique column/constraint or index into [`IndexModel::predicate`].
///
/// [`IndexModel::predicate`]: crate::model::IndexModel::predicate
pub fn render_ddl_predicate<K, Ast>(predicate: &Predicate<'_, K, Ast>) -> String
where
    K: PredicateKind,
    Ast: PredicateAst + DdlPredicateAst,
{
    let mut out = String::new();
    predicate.ast.render_ddl(&mut out);
    out
}

/// A typed SQL scalar expression scoped to a query builder invocation.
#[derive(Debug, PartialEq)]
pub struct Expr<'scope, K, Ast = ColumnExprAst<K>>
where
    Ast: ExprAst,
{
    ast: Ast,
    project_alias: Cow<'static, str>,
    _phantom: PhantomData<(&'scope (), K)>,
}

/// A copyable reference to a source column scoped to a query builder invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct ColumnRef<'scope, K> {
    alias: SourceAlias,
    column: &'static str,
    project_alias: &'static str,
    _phantom: PhantomData<(&'scope (), K)>,
}

impl<'scope, K> Clone for ColumnRef<'scope, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'scope, K> Copy for ColumnRef<'scope, K> {}

/// The predicate produced by a [`ColumnRef`] comparison helper (`equals`, `less_than`, ...):
/// a comparison between this column's expression and the right-hand side's AST.
pub type ColumnComparison<'scope, Cmp, K, RhsAst> =
    Predicate<'scope, Cmp, ComparePredicateAst<ColumnExprAst<K>, RhsAst>>;

/// The predicate produced by a [`ColumnRef`] `like`/`ilike` helper: a pattern match between this
/// column's expression and the pattern's AST.
pub type ColumnLike<'scope, K, RhsKind, RhsAst> =
    Predicate<'scope, LikePredicate<K, RhsKind>, LikePredicateAst<ColumnExprAst<K>, RhsAst>>;

/// The predicate produced by a [`ColumnRef`] `between` helper: a range test of this column's
/// expression against the `lo`/`hi` ASTs.
pub type ColumnBetween<'scope, K, LoAst, HiAst> =
    Predicate<'scope, BetweenPredicate<K>, BetweenPredicateAst<ColumnExprAst<K>, LoAst, HiAst>>;

impl<'scope, K> ColumnRef<'scope, K>
where
    K: ExprKind,
{
    #[doc(hidden)]
    pub fn column(alias: SourceAlias, column: &'static str) -> Self {
        Self::column_with_project_alias(alias, column, column)
    }

    #[doc(hidden)]
    pub fn column_with_project_alias(
        alias: SourceAlias,
        column: &'static str,
        project_alias: &'static str,
    ) -> Self {
        Self {
            alias,
            column,
            project_alias,
            _phantom: PhantomData,
        }
    }

    /// Materialize this column reference as an owned typed expression.
    pub fn into_expr(self) -> Expr<'scope, K> {
        Expr::column_with_project_alias(self.alias, self.column, self.project_alias)
    }

    /// Re-type this column reference as nullable (`ColumnRef<Nullable<K>>`), preserving alias, column,
    /// and project alias. Used to nullable-wrap the accumulated base columns when building a
    /// `RIGHT`/`FULL JOIN` (the kind is purely phantom, so this is a no-op rewrap).
    #[doc(hidden)]
    pub fn into_nullable(self) -> ColumnRef<'scope, Nullable<K>> {
        ColumnRef {
            alias: self.alias,
            column: self.column,
            project_alias: self.project_alias,
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn visit<V>(self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
    {
        visitor.visit_column(self.alias, self.column)
    }

    /// The default output alias when this column is selected directly.
    pub fn project_alias(self) -> &'static str {
        self.project_alias
    }

    /// SQL equality.
    pub fn equals<'other, R>(
        self,
        other: R,
    ) -> ColumnComparison<'scope, EqualsPredicate<K, R::Kind>, K, R::Ast>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().equals(other)
    }

    /// SQL inequality.
    pub fn not_equals<'other, R>(
        self,
        other: R,
    ) -> ColumnComparison<'scope, NotEqualsPredicate<K, R::Kind>, K, R::Ast>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().not_equals(other)
    }

    /// SQL less-than comparison.
    pub fn less_than<'other, R>(
        self,
        other: R,
    ) -> ColumnComparison<'scope, LessThanPredicate<K, R::Kind>, K, R::Ast>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().less_than(other)
    }

    /// SQL less-than-or-equal comparison.
    pub fn less_than_or_equals<'other, R>(
        self,
        other: R,
    ) -> ColumnComparison<'scope, LessThanOrEqualsPredicate<K, R::Kind>, K, R::Ast>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().less_than_or_equals(other)
    }

    /// SQL greater-than comparison.
    pub fn greater_than<'other, R>(
        self,
        other: R,
    ) -> ColumnComparison<'scope, GreaterThanPredicate<K, R::Kind>, K, R::Ast>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().greater_than(other)
    }

    /// SQL greater-than-or-equal comparison.
    pub fn greater_than_or_equals<'other, R>(
        self,
        other: R,
    ) -> ColumnComparison<'scope, GreaterThanOrEqualsPredicate<K, R::Kind>, K, R::Ast>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().greater_than_or_equals(other)
    }

    /// SQL `LIKE` pattern match (text columns only).
    pub fn like<'other, R>(self, pattern: R) -> ColumnLike<'scope, K, R::Kind, R::Ast>
    where
        K::Value: SqlText,
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().like(pattern)
    }

    /// SQL `NOT LIKE` pattern match (text columns only).
    pub fn not_like<'other, R>(self, pattern: R) -> ColumnLike<'scope, K, R::Kind, R::Ast>
    where
        K::Value: SqlText,
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().not_like(pattern)
    }

    /// SQL case-insensitive `ILIKE` pattern match (text columns only).
    pub fn ilike<'other, R>(self, pattern: R) -> ColumnLike<'scope, K, R::Kind, R::Ast>
    where
        K::Value: SqlText,
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().ilike(pattern)
    }

    /// SQL case-insensitive `NOT ILIKE` pattern match (text columns only).
    pub fn not_ilike<'other, R>(self, pattern: R) -> ColumnLike<'scope, K, R::Kind, R::Ast>
    where
        K::Value: SqlText,
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().not_ilike(pattern)
    }

    /// SQL `IN (...)` membership against an inline value list.
    pub fn in_<I>(
        self,
        values: I,
    ) -> Predicate<'scope, InPredicate<K>, InPredicateAst<ColumnExprAst<K>, K::Value>>
    where
        I: IntoIterator,
        I::Item: Into<K::Value>,
        K::Value: Clone,
    {
        self.into_expr().in_(values)
    }

    /// SQL `NOT IN (...)` membership against an inline value list.
    pub fn not_in<I>(
        self,
        values: I,
    ) -> Predicate<'scope, InPredicate<K>, InPredicateAst<ColumnExprAst<K>, K::Value>>
    where
        I: IntoIterator,
        I::Item: Into<K::Value>,
        K::Value: Clone,
    {
        self.into_expr().not_in(values)
    }

    /// SQL `IN (subquery)` against a single-column subquery of this column's value type.
    pub fn in_subquery<Sub>(
        self,
        subquery: Sub,
    ) -> Predicate<'scope, InPredicate<K>, InSubqueryPredicateAst<ColumnExprAst<K>, Sub>>
    where
        Sub: crate::ScalarSubquery,
        Sub::OutputKind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<Sub::Params>,
    {
        self.into_expr().in_subquery(subquery)
    }

    /// SQL `NOT IN (subquery)`; see [`in_subquery`](Self::in_subquery).
    pub fn not_in_subquery<Sub>(
        self,
        subquery: Sub,
    ) -> Predicate<'scope, InPredicate<K>, InSubqueryPredicateAst<ColumnExprAst<K>, Sub>>
    where
        Sub: crate::ScalarSubquery,
        Sub::OutputKind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<Sub::Params>,
    {
        self.into_expr().not_in_subquery(subquery)
    }

    /// SQL `BETWEEN lo AND hi` (inclusive).
    pub fn between<'other, Lo, Hi>(
        self,
        lo: Lo,
        hi: Hi,
    ) -> ColumnBetween<'scope, K, Lo::Ast, Hi::Ast>
    where
        Lo: IntoExpr<'other>,
        Hi: IntoExpr<'other>,
        Lo::Kind: ExprKind<Value = K::Value>,
        Hi::Kind: ExprKind<Value = K::Value>,
        // `ColumnExprAst` carries no params (`HNil`), so the operand-append collapses and only the
        // `lo`/`hi` params need to concatenate.
        <Lo::Ast as ExprAst>::Params: crate::HAppend<<Hi::Ast as ExprAst>::Params>,
    {
        self.into_expr().between(lo, hi)
    }

    /// SQL `NOT BETWEEN lo AND hi`.
    pub fn not_between<'other, Lo, Hi>(
        self,
        lo: Lo,
        hi: Hi,
    ) -> ColumnBetween<'scope, K, Lo::Ast, Hi::Ast>
    where
        Lo: IntoExpr<'other>,
        Hi: IntoExpr<'other>,
        Lo::Kind: ExprKind<Value = K::Value>,
        Hi::Kind: ExprKind<Value = K::Value>,
        <Lo::Ast as ExprAst>::Params: crate::HAppend<<Hi::Ast as ExprAst>::Params>,
    {
        self.into_expr().not_between(lo, hi)
    }

    /// SQL `COUNT(column)`.
    pub fn count(self) -> Expr<'scope, CountExpr<K>, AggregateExprAst<ColumnExprAst<K>>> {
        self.into_expr().count()
    }

    /// SQL `SUM(column)` (numeric columns; integer sums widen per [`SqlSum`] — to `i64` for
    /// ≤32-bit operands, `i128` for 64-bit and wider). Also accepts nullable / left-joined columns.
    pub fn sum(self) -> Expr<'scope, SumExpr<K>, AggregateExprAst<ColumnExprAst<K>>>
    where
        K::Value: AggregateScalar,
        <K::Value as AggregateScalar>::Scalar: SqlSum,
    {
        self.into_expr().sum()
    }

    /// SQL `AVG(column)` (numeric columns), producing `Option<f64>`.
    pub fn avg(self) -> Expr<'scope, AvgExpr<K>, AggregateExprAst<ColumnExprAst<K>>>
    where
        K::Value: AggregateScalar,
        <K::Value as AggregateScalar>::Scalar: SqlNumber,
    {
        self.into_expr().avg()
    }

    /// SQL `MIN(column)`.
    pub fn min(self) -> Expr<'scope, MinExpr<K>, AggregateExprAst<ColumnExprAst<K>>>
    where
        K::Value: AggregateScalar,
    {
        self.into_expr().min()
    }

    /// SQL `MAX(column)`.
    pub fn max(self) -> Expr<'scope, MaxExpr<K>, AggregateExprAst<ColumnExprAst<K>>>
    where
        K::Value: AggregateScalar,
    {
        self.into_expr().max()
    }

    /// Sort by this column in ascending order.
    pub fn asc(self) -> Order<'scope, K, ColumnExprAst<K>> {
        self.into_expr().asc()
    }

    /// Sort by this column in descending order.
    pub fn desc(self) -> Order<'scope, K, ColumnExprAst<K>> {
        self.into_expr().desc()
    }
}

/// `IS NULL` / `IS NOT NULL` tests, available only on nullable columns (`K: NullableExpr`).
impl<'scope, K> ColumnRef<'scope, K>
where
    K: ExprKind + NullableExpr,
{
    /// SQL `IS NULL`.
    pub fn is_null(
        self,
    ) -> Predicate<'scope, IsNullPredicate<K>, NullCheckPredicateAst<ColumnExprAst<K>>> {
        self.into_expr().is_null()
    }

    /// SQL `IS NOT NULL`.
    pub fn is_not_null(
        self,
    ) -> Predicate<'scope, IsNotNullPredicate<K>, NullCheckPredicateAst<ColumnExprAst<K>>> {
        self.into_expr().is_not_null()
    }
}

/// Boolean columns can be used directly as a predicate (`K::Value = bool`).
impl<'scope, K> ColumnRef<'scope, K>
where
    K: ExprKind<Value = bool>,
{
    /// Use this boolean column as a predicate (matches rows where it is true).
    pub fn is_true(
        self,
    ) -> Predicate<'scope, BoolTestPredicate<K>, BoolTestPredicateAst<ColumnExprAst<K>>> {
        self.into_expr().is_true()
    }

    /// Use the negation of this boolean column as a predicate (matches rows where it is false).
    pub fn is_false(
        self,
    ) -> Predicate<'scope, BoolTestPredicate<K>, BoolTestPredicateAst<ColumnExprAst<K>>> {
        self.into_expr().is_false()
    }
}

// Numeric arithmetic on a `ColumnRef` is provided by the `Add`/`Sub`/`Mul`/`Div` operator impls
// below (`column + other`, etc.); the equivalent inherent helpers were redundant with them.

impl<'scope, K> Expr<'scope, K, ColumnExprAst<K>>
where
    K: ExprKind,
{
    #[doc(hidden)]
    pub fn column(alias: SourceAlias, column: impl Into<Cow<'static, str>>) -> Self {
        let column = column.into();
        let project_alias = column.clone();
        Self {
            ast: ColumnExprAst {
                alias,
                column,
                _kind: PhantomData,
            },
            project_alias,
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn column_with_project_alias(
        alias: SourceAlias,
        column: impl Into<Cow<'static, str>>,
        project_alias: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            ast: ColumnExprAst {
                alias,
                column: column.into(),
                _kind: PhantomData,
            },
            project_alias: project_alias.into(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, K> Expr<'scope, K, LiteralExprAst<K>>
where
    K: ExprKind,
    K::Value: Clone,
{
    /// Construct a SQL literal expression.
    ///
    /// The literal's value is carried in the AST and encoded as a bound parameter at
    /// render time via [`Encode`](crate::Encode), so the literal can be any type the
    /// target backend knows how to encode.
    pub fn lit(value: impl Into<K::Value>) -> Self {
        Self {
            ast: LiteralExprAst {
                value: value.into(),
                _kind: PhantomData,
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, K> Expr<'scope, RuntimeParam<K>, ParamExprAst<K>>
where
    K: ExprKind,
{
    /// Construct a prepared statement runtime parameter expression.
    pub fn param() -> Self {
        Self {
            ast: ParamExprAst { _kind: PhantomData },
            project_alias: Cow::Borrowed("param"),
            _phantom: PhantomData,
        }
    }
}

/// Construct a prepared statement runtime parameter expression.
pub fn param<'scope, K>() -> Expr<'scope, RuntimeParam<K>, ParamExprAst<K>>
where
    K: ExprKind,
{
    Expr::param()
}

impl<'scope, K, Ast> Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    #[doc(hidden)]
    pub fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
        Ast: RenderAst<V::Backend>,
    {
        self.ast.visit(visitor)
    }

    /// The default output alias when this expression is selected directly.
    pub fn project_alias(&self) -> &str {
        &self.project_alias
    }

    /// SQL equality.
    pub fn equals<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, EqualsPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::Equals,
            right: other.into_expr().ast,
        })
    }

    /// SQL inequality.
    pub fn not_equals<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, NotEqualsPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::NotEquals,
            right: other.into_expr().ast,
        })
    }

    /// SQL less-than comparison.
    pub fn less_than<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, LessThanPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::LessThan,
            right: other.into_expr().ast,
        })
    }

    /// SQL less-than-or-equal comparison.
    pub fn less_than_or_equals<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, LessThanOrEqualsPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::LessThanOrEquals,
            right: other.into_expr().ast,
        })
    }

    /// SQL greater-than comparison.
    pub fn greater_than<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, GreaterThanPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::GreaterThan,
            right: other.into_expr().ast,
        })
    }

    /// SQL greater-than-or-equal comparison.
    pub fn greater_than_or_equals<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, GreaterThanOrEqualsPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::GreaterThanOrEquals,
            right: other.into_expr().ast,
        })
    }

    /// SQL `LIKE` pattern match. Available only on text expressions; the pattern is any text
    /// operand (a `&str`/`String` literal, a runtime param, or another text column).
    pub fn like<'other, R>(
        &self,
        pattern: R,
    ) -> Predicate<'scope, LikePredicate<K, R::Kind>, LikePredicateAst<Ast, R::Ast>>
    where
        K::Value: SqlText,
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.like_impl(pattern, false, false)
    }

    /// SQL `NOT LIKE` pattern match.
    pub fn not_like<'other, R>(
        &self,
        pattern: R,
    ) -> Predicate<'scope, LikePredicate<K, R::Kind>, LikePredicateAst<Ast, R::Ast>>
    where
        K::Value: SqlText,
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.like_impl(pattern, false, true)
    }

    /// SQL case-insensitive `ILIKE` pattern match (case-insensitive `LIKE` on dialects without
    /// `ILIKE`).
    pub fn ilike<'other, R>(
        &self,
        pattern: R,
    ) -> Predicate<'scope, LikePredicate<K, R::Kind>, LikePredicateAst<Ast, R::Ast>>
    where
        K::Value: SqlText,
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.like_impl(pattern, true, false)
    }

    /// SQL case-insensitive `NOT ILIKE` pattern match.
    pub fn not_ilike<'other, R>(
        &self,
        pattern: R,
    ) -> Predicate<'scope, LikePredicate<K, R::Kind>, LikePredicateAst<Ast, R::Ast>>
    where
        K::Value: SqlText,
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.like_impl(pattern, true, true)
    }

    fn like_impl<'other, R>(
        &self,
        pattern: R,
        case_insensitive: bool,
        negated: bool,
    ) -> Predicate<'scope, LikePredicate<K, R::Kind>, LikePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(LikePredicateAst {
            left: self.ast.clone(),
            right: pattern.into_expr().ast,
            case_insensitive,
            negated,
        })
    }

    /// SQL `IN (...)` membership against an inline list of values of this expression's value type.
    /// An empty list matches no rows.
    pub fn in_<I>(
        &self,
        values: I,
    ) -> Predicate<'scope, InPredicate<K>, InPredicateAst<Ast, K::Value>>
    where
        I: IntoIterator,
        I::Item: Into<K::Value>,
        K::Value: Clone,
    {
        self.in_impl(values, false)
    }

    /// SQL `NOT IN (...)` membership. An empty list matches every row.
    pub fn not_in<I>(
        &self,
        values: I,
    ) -> Predicate<'scope, InPredicate<K>, InPredicateAst<Ast, K::Value>>
    where
        I: IntoIterator,
        I::Item: Into<K::Value>,
        K::Value: Clone,
    {
        self.in_impl(values, true)
    }

    fn in_impl<I>(
        &self,
        values: I,
        negated: bool,
    ) -> Predicate<'scope, InPredicate<K>, InPredicateAst<Ast, K::Value>>
    where
        I: IntoIterator,
        I::Item: Into<K::Value>,
        K::Value: Clone,
    {
        Predicate::new(InPredicateAst {
            operand: self.ast.clone(),
            values: values.into_iter().map(Into::into).collect(),
            negated,
        })
    }

    /// SQL `IN (subquery)`: membership against a subquery that projects exactly one column whose
    /// kind matches this expression's value type. Matching by value type means a `ColumnType`
    /// newtype is enforced and a nullable projected column (whose kind's value is the non-null inner
    /// type) is accepted, as in SQL. The subquery may be correlated.
    pub fn in_subquery<Sub>(
        &self,
        subquery: Sub,
    ) -> Predicate<'scope, InPredicate<K>, InSubqueryPredicateAst<Ast, Sub>>
    where
        Sub: crate::ScalarSubquery,
        Sub::OutputKind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<Sub::Params>,
    {
        self.in_subquery_impl(subquery, false)
    }

    /// SQL `NOT IN (subquery)`; see [`in_subquery`](Self::in_subquery).
    pub fn not_in_subquery<Sub>(
        &self,
        subquery: Sub,
    ) -> Predicate<'scope, InPredicate<K>, InSubqueryPredicateAst<Ast, Sub>>
    where
        Sub: crate::ScalarSubquery,
        Sub::OutputKind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<Sub::Params>,
    {
        self.in_subquery_impl(subquery, true)
    }

    fn in_subquery_impl<Sub>(
        &self,
        subquery: Sub,
        negated: bool,
    ) -> Predicate<'scope, InPredicate<K>, InSubqueryPredicateAst<Ast, Sub>>
    where
        Sub: crate::ScalarSubquery,
        Sub::OutputKind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<Sub::Params>,
    {
        Predicate::new(InSubqueryPredicateAst {
            operand: self.ast.clone(),
            subquery,
            negated,
        })
    }

    /// SQL `BETWEEN lo AND hi` (inclusive). `lo` and `hi` are any operands of this expression's
    /// value type.
    pub fn between<'other, Lo, Hi>(
        &self,
        lo: Lo,
        hi: Hi,
    ) -> Predicate<'scope, BetweenPredicate<K>, BetweenPredicateAst<Ast, Lo::Ast, Hi::Ast>>
    where
        Lo: IntoExpr<'other>,
        Hi: IntoExpr<'other>,
        Lo::Kind: ExprKind<Value = K::Value>,
        Hi::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<Lo::Ast as ExprAst>::Params>,
        <Ast::Params as crate::HAppend<<Lo::Ast as ExprAst>::Params>>::Output:
            crate::HAppend<<Hi::Ast as ExprAst>::Params>,
    {
        self.between_impl(lo, hi, false)
    }

    /// SQL `NOT BETWEEN lo AND hi`.
    pub fn not_between<'other, Lo, Hi>(
        &self,
        lo: Lo,
        hi: Hi,
    ) -> Predicate<'scope, BetweenPredicate<K>, BetweenPredicateAst<Ast, Lo::Ast, Hi::Ast>>
    where
        Lo: IntoExpr<'other>,
        Hi: IntoExpr<'other>,
        Lo::Kind: ExprKind<Value = K::Value>,
        Hi::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<Lo::Ast as ExprAst>::Params>,
        <Ast::Params as crate::HAppend<<Lo::Ast as ExprAst>::Params>>::Output:
            crate::HAppend<<Hi::Ast as ExprAst>::Params>,
    {
        self.between_impl(lo, hi, true)
    }

    fn between_impl<'other, Lo, Hi>(
        &self,
        lo: Lo,
        hi: Hi,
        negated: bool,
    ) -> Predicate<'scope, BetweenPredicate<K>, BetweenPredicateAst<Ast, Lo::Ast, Hi::Ast>>
    where
        Lo: IntoExpr<'other>,
        Hi: IntoExpr<'other>,
        Ast::Params: crate::HAppend<<Lo::Ast as ExprAst>::Params>,
        <Ast::Params as crate::HAppend<<Lo::Ast as ExprAst>::Params>>::Output:
            crate::HAppend<<Hi::Ast as ExprAst>::Params>,
    {
        Predicate::new(BetweenPredicateAst {
            operand: self.ast.clone(),
            lo: lo.into_expr().ast,
            hi: hi.into_expr().ast,
            negated,
        })
    }

    /// SQL `COUNT(expr)` — counts non-null values of this expression (never `NULL`; `0` for an
    /// empty input), producing an `i64`. The operand must be aggregate-free (`Ast: NonAggregateAst`)
    /// so an aggregate cannot be nested inside another (`SUM(COUNT(...))` is invalid SQL).
    pub fn count(&self) -> Expr<'scope, CountExpr<K>, AggregateExprAst<Ast>>
    where
        Ast: NonAggregateAst,
    {
        self.aggregate(AggregateFunc::Count, None)
    }

    /// SQL `SUM(expr)` — `NULL` over an empty input, so the result is `Option<…>`; integer sums
    /// widen per [`SqlSum`] (`i64` for ≤32-bit operands, `i128` for 64-bit and wider). The call is
    /// cast to that type so the database's own result (which can be `numeric`) decodes correctly.
    /// Works on nullable / left-joined operands, which aggregate over the same scalar as their
    /// non-null counterpart (see [`AggregateScalar`]).
    pub fn sum(&self) -> Expr<'scope, SumExpr<K>, AggregateExprAst<Ast>>
    where
        Ast: NonAggregateAst,
        K::Value: AggregateScalar,
        <K::Value as AggregateScalar>::Scalar: SqlSum,
    {
        self.aggregate(
            AggregateFunc::Sum,
            Some(<<K::Value as AggregateScalar>::Scalar as SqlSum>::SUM_CAST),
        )
    }

    /// SQL `AVG(expr)` — `NULL` over an empty input and always fractional, so the result is
    /// `Option<f64>`. Cast to `double precision` since the database returns `numeric` for integer
    /// inputs.
    pub fn avg(&self) -> Expr<'scope, AvgExpr<K>, AggregateExprAst<Ast>>
    where
        Ast: NonAggregateAst,
        K::Value: AggregateScalar,
        <K::Value as AggregateScalar>::Scalar: SqlNumber,
    {
        self.aggregate(AggregateFunc::Avg, Some(crate::SqlType::F64))
    }

    /// SQL `MIN(expr)` — `NULL` over an empty input, so the result is `Option<…>` of the operand's
    /// scalar (a nullable operand does not nest a second `Option`).
    pub fn min(&self) -> Expr<'scope, MinExpr<K>, AggregateExprAst<Ast>>
    where
        Ast: NonAggregateAst,
        K::Value: AggregateScalar,
    {
        self.aggregate(AggregateFunc::Min, None)
    }

    /// SQL `MAX(expr)` — `NULL` over an empty input, so the result is `Option<…>`.
    pub fn max(&self) -> Expr<'scope, MaxExpr<K>, AggregateExprAst<Ast>>
    where
        Ast: NonAggregateAst,
        K::Value: AggregateScalar,
    {
        self.aggregate(AggregateFunc::Max, None)
    }

    fn aggregate<ResultKind>(
        &self,
        func: AggregateFunc,
        cast: Option<crate::SqlType>,
    ) -> Expr<'scope, ResultKind, AggregateExprAst<Ast>> {
        Expr {
            ast: AggregateExprAst {
                func,
                cast,
                operand: self.ast.clone(),
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }

    /// Sort by this expression in ascending order.
    pub fn asc(&self) -> Order<'scope, K, Ast> {
        Order::new(self.ast.clone(), OrderDirection::Asc)
    }

    /// Sort by this expression in descending order.
    pub fn desc(&self) -> Order<'scope, K, Ast> {
        Order::new(self.ast.clone(), OrderDirection::Desc)
    }
}

/// `IS NULL` / `IS NOT NULL` tests are only available on nullable expressions (`K: NullableExpr`);
/// calling them on a column the type system knows is `NOT NULL` is a compile error, since such a
/// test would be a constant.
impl<'scope, K, Ast> Expr<'scope, K, Ast>
where
    K: ExprKind + NullableExpr,
    Ast: ExprAst,
{
    /// SQL `IS NULL`.
    pub fn is_null(&self) -> Predicate<'scope, IsNullPredicate<K>, NullCheckPredicateAst<Ast>> {
        Predicate::new(NullCheckPredicateAst {
            operand: self.ast.clone(),
            negated: false,
        })
    }

    /// SQL `IS NOT NULL`.
    pub fn is_not_null(
        &self,
    ) -> Predicate<'scope, IsNotNullPredicate<K>, NullCheckPredicateAst<Ast>> {
        Predicate::new(NullCheckPredicateAst {
            operand: self.ast.clone(),
            negated: true,
        })
    }
}

/// Using a boolean-valued expression directly as a predicate. Available only on non-null `bool`
/// expressions, so a bool column can go straight into `where_` without an explicit `.equals(true)`.
impl<'scope, K, Ast> Expr<'scope, K, Ast>
where
    K: ExprKind<Value = bool>,
    Ast: ExprAst,
{
    /// Use this boolean expression as a predicate (matches rows where it is true).
    pub fn is_true(&self) -> Predicate<'scope, BoolTestPredicate<K>, BoolTestPredicateAst<Ast>> {
        Predicate::new(BoolTestPredicateAst {
            operand: self.ast.clone(),
            negated: false,
        })
    }

    /// Use the negation of this boolean expression as a predicate (matches rows where it is false).
    pub fn is_false(&self) -> Predicate<'scope, BoolTestPredicate<K>, BoolTestPredicateAst<Ast>> {
        Predicate::new(BoolTestPredicateAst {
            operand: self.ast.clone(),
            negated: true,
        })
    }
}

impl<'scope, K, Ast> Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
{
    /// SQL numeric addition.
    pub fn add<R>(&self, other: R) -> Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Self::binary::<AddExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Add, other)
    }

    /// SQL numeric subtraction.
    pub fn subtract<R>(
        &self,
        other: R,
    ) -> Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Self::binary::<SubtractExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Subtract, other)
    }

    /// SQL numeric multiplication.
    pub fn multiply<R>(
        &self,
        other: R,
    ) -> Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Self::binary::<MultiplyExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Multiply, other)
    }

    /// SQL numeric division.
    pub fn divide<R>(
        &self,
        other: R,
    ) -> Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        K::Value: SqlDivide,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Self::binary::<DivideExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Divide, other)
    }

    fn binary<ResultKind, R>(
        left: Ast,
        op: ArithmeticOp,
        right: R,
    ) -> Expr<'scope, ResultKind, BinaryExprAst<Ast, R::Ast>>
    where
        ResultKind: ExprKind,
        R: IntoExpr<'scope>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        let right = right.into_expr();
        Expr {
            ast: BinaryExprAst {
                left,
                op,
                right: right.ast,
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, K, Ast> Clone for Expr<'scope, K, Ast>
where
    Ast: ExprAst,
{
    fn clone(&self) -> Self {
        Self {
            ast: self.ast.clone(),
            project_alias: self.project_alias.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, K, Ast, R> Add<R> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn add(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<AddExpr<K, R::Kind>, _>(self.ast, ArithmeticOp::Add, other)
    }
}

impl<'scope, K, Ast, R> Add<R> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn add(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<AddExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Add, other)
    }
}

impl<'scope, K, R> Add<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn add(self, other: R) -> Self::Output {
        self.into_expr() + other
    }
}

impl<'scope, K, R> Add<R> for &ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn add(self, other: R) -> Self::Output {
        (*self).into_expr() + other
    }
}

impl<'scope, K, Ast, R> Sub<R> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn sub(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<SubtractExpr<K, R::Kind>, _>(
            self.ast,
            ArithmeticOp::Subtract,
            other,
        )
    }
}

impl<'scope, K, Ast, R> Sub<R> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn sub(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<SubtractExpr<K, R::Kind>, _>(
            self.ast.clone(),
            ArithmeticOp::Subtract,
            other,
        )
    }
}

impl<'scope, K, R> Sub<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn sub(self, other: R) -> Self::Output {
        self.into_expr() - other
    }
}

impl<'scope, K, R> Sub<R> for &ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn sub(self, other: R) -> Self::Output {
        (*self).into_expr() - other
    }
}

impl<'scope, K, Ast, R> Mul<R> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn mul(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<MultiplyExpr<K, R::Kind>, _>(
            self.ast,
            ArithmeticOp::Multiply,
            other,
        )
    }
}

impl<'scope, K, Ast, R> Mul<R> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn mul(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<MultiplyExpr<K, R::Kind>, _>(
            self.ast.clone(),
            ArithmeticOp::Multiply,
            other,
        )
    }
}

impl<'scope, K, R> Mul<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn mul(self, other: R) -> Self::Output {
        self.into_expr() * other
    }
}

impl<'scope, K, R> Mul<R> for &ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn mul(self, other: R) -> Self::Output {
        (*self).into_expr() * other
    }
}

impl<'scope, K, Ast, R> Div<R> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlDivide,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn div(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<DivideExpr<K, R::Kind>, _>(self.ast, ArithmeticOp::Divide, other)
    }
}

impl<'scope, K, Ast, R> Div<R> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlDivide,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn div(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<DivideExpr<K, R::Kind>, _>(
            self.ast.clone(),
            ArithmeticOp::Divide,
            other,
        )
    }
}

impl<'scope, K, R> Div<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlDivide,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn div(self, other: R) -> Self::Output {
        self.into_expr() / other
    }
}

impl<'scope, K, R> Div<R> for &ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlDivide,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn div(self, other: R) -> Self::Output {
        (*self).into_expr() / other
    }
}

macro_rules! impl_primitive_left_arithmetic {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<'scope, K, Ast> Add<Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn add(self, other: Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) + other
                }
            }

            impl<'scope, K, Ast> Add<&Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn add(self, other: &Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) + other
                }
            }

            impl<'scope, K> Add<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn add(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) + other
                }
            }

            impl<'scope, K> Add<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn add(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) + other
                }
            }

            impl<'scope, K, Ast> Sub<Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn sub(self, other: Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) - other
                }
            }

            impl<'scope, K, Ast> Sub<&Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn sub(self, other: &Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) - other
                }
            }

            impl<'scope, K> Sub<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn sub(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) - other
                }
            }

            impl<'scope, K> Sub<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn sub(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) - other
                }
            }

            impl<'scope, K, Ast> Mul<Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn mul(self, other: Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) * other
                }
            }

            impl<'scope, K, Ast> Mul<&Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn mul(self, other: &Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) * other
                }
            }

            impl<'scope, K> Mul<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn mul(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) * other
                }
            }

            impl<'scope, K> Mul<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn mul(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) * other
                }
            }

            impl<'scope, K, Ast> Div<Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn div(self, other: Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) / other
                }
            }

            impl<'scope, K, Ast> Div<&Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn div(self, other: &Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) / other
                }
            }

            impl<'scope, K> Div<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn div(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) / other
                }
            }

            impl<'scope, K> Div<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn div(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) / other
                }
            }
        )*
    };
}

impl_primitive_left_arithmetic!(i8, i16, i32, i64, i128, isize);
impl_primitive_left_arithmetic!(u8, u16, u32, u64, u128, usize);
impl_primitive_left_arithmetic!(f32, f64);

/// Converts Rust values into scoped SQL expressions.
pub trait IntoExpr<'scope> {
    type Kind: ExprKind;
    type Ast: ExprAst;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast>;
}

impl<'scope, K, Ast> IntoExpr<'scope> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    type Kind = K;
    type Ast = Ast;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        self
    }
}

impl<'scope, K, Ast> IntoExpr<'scope> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    type Kind = K;
    type Ast = Ast;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        self.clone()
    }
}

impl<'scope, K> IntoExpr<'scope> for ColumnRef<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;
    type Ast = ColumnExprAst<K>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        self.into_expr()
    }
}

impl<'scope, K> IntoExpr<'scope> for &ColumnRef<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;
    type Ast = ColumnExprAst<K>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        (*self).into_expr()
    }
}

impl<'scope, T> IntoExpr<'scope> for T
where
    T: ExprKind<Value = T> + Clone,
{
    type Kind = T;
    type Ast = LiteralExprAst<T>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope> for &str {
    type Kind = String;
    type Ast = LiteralExprAst<String>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope> for &String {
    type Kind = String;
    type Ast = LiteralExprAst<String>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        Expr::lit(self)
    }
}

// Borrowed `bytea`/`BLOB` literals, mirroring `&str`/`&String` (both convert to the owned `Vec<u8>`
// the literal stores). Lets `col.equals(&bytes)` / `col.equals(&bytes[..])` skip a caller-side clone.
impl<'scope> IntoExpr<'scope> for &[u8] {
    type Kind = Vec<u8>;
    type Ast = LiteralExprAst<Vec<u8>>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope> for &Vec<u8> {
    type Kind = Vec<u8>;
    type Ast = LiteralExprAst<Vec<u8>>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        Expr::lit(self.clone())
    }
}

/// A typed SQL ordering expression scoped to a query builder invocation.
#[derive(Debug, PartialEq)]
pub struct Order<'scope, K, Ast>
where
    Ast: ExprAst,
{
    ast: Ast,
    direction: OrderDirection,
    _phantom: PhantomData<(&'scope (), K)>,
}

impl<'scope, K, Ast> Order<'scope, K, Ast>
where
    Ast: ExprAst,
{
    fn new(ast: Ast, direction: OrderDirection) -> Self {
        Self {
            ast,
            direction,
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn visit_expr<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
        Ast: RenderAst<V::Backend>,
    {
        self.ast.visit(visitor)
    }

    #[doc(hidden)]
    pub fn direction(&self) -> OrderDirection {
        self.direction
    }
}

impl<'scope, K, Ast> Clone for Order<'scope, K, Ast>
where
    Ast: ExprAst,
{
    fn clone(&self) -> Self {
        Self {
            ast: self.ast.clone(),
            direction: self.direction,
            _phantom: PhantomData,
        }
    }
}

/// A typed SQL boolean predicate scoped to a query builder invocation.
///
/// `WHERE` clauses require predicates rather than arbitrary scalar expressions:
///
/// ```compile_fail
/// use squealy::*;
/// use squealy_test::TestConnection;
///
/// #[derive(Clone, Table)]
/// struct User<'scope, C: ColumnMode = ColumnExpr> {
///     id: C::Type<'scope, i32>,
/// }
///
/// let conn = TestConnection;
/// let _ = conn
///     .from::<User>()
///     .where_(|(user,)| user.id)
///     .select(|(user,)| user);
/// ```
#[derive(Debug, PartialEq)]
pub struct Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    ast: Ast,
    _phantom: PhantomData<(&'scope (), K)>,
}

impl<'scope, K, Ast> Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    fn new(ast: Ast) -> Self {
        Self {
            ast,
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor,
        Ast: RenderPredicateAst<V::Backend>,
    {
        self.ast.visit(visitor)
    }

    /// SQL conjunction.
    pub fn and<'other, R, OtherAst>(
        &self,
        other: Predicate<'other, R, OtherAst>,
    ) -> Predicate<'scope, AndPredicate<K, R>, AndPredicateAst<Ast, OtherAst>>
    where
        R: PredicateKind,
        OtherAst: PredicateAst,
        Ast::Params: crate::HAppend<OtherAst::Params>,
    {
        Predicate::new(AndPredicateAst {
            left: self.ast.clone(),
            right: other.ast,
        })
    }

    /// SQL disjunction.
    pub fn or<'other, R, OtherAst>(
        &self,
        other: Predicate<'other, R, OtherAst>,
    ) -> Predicate<'scope, OrPredicate<K, R>, OrPredicateAst<Ast, OtherAst>>
    where
        R: PredicateKind,
        OtherAst: PredicateAst,
        Ast::Params: crate::HAppend<OtherAst::Params>,
    {
        Predicate::new(OrPredicateAst {
            left: self.ast.clone(),
            right: other.ast,
        })
    }

    /// SQL negation.
    pub fn not_(&self) -> Predicate<'scope, NotPredicate<K>, NotPredicateAst<Ast>> {
        Predicate::new(NotPredicateAst {
            predicate: self.ast.clone(),
        })
    }
}

impl<'scope, K, Ast> Clone for Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    fn clone(&self) -> Self {
        Self {
            ast: self.ast.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, L, LeftAst, R, RightAst> BitAnd<Predicate<'scope, R, RightAst>>
    for Predicate<'scope, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, AndPredicate<L, R>, AndPredicateAst<LeftAst, RightAst>>;

    fn bitand(self, rhs: Predicate<'scope, R, RightAst>) -> Self::Output {
        Predicate::new(AndPredicateAst {
            left: self.ast,
            right: rhs.ast,
        })
    }
}

impl<'scope, 'rhs, L, LeftAst, R, RightAst> BitAnd<&Predicate<'rhs, R, RightAst>>
    for Predicate<'scope, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, AndPredicate<L, R>, AndPredicateAst<LeftAst, RightAst>>;

    fn bitand(self, rhs: &Predicate<'rhs, R, RightAst>) -> Self::Output {
        Predicate::new(AndPredicateAst {
            left: self.ast,
            right: rhs.ast.clone(),
        })
    }
}

impl<'scope, 'lhs, L, LeftAst, R, RightAst> BitAnd<Predicate<'scope, R, RightAst>>
    for &Predicate<'lhs, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, AndPredicate<L, R>, AndPredicateAst<LeftAst, RightAst>>;

    fn bitand(self, rhs: Predicate<'scope, R, RightAst>) -> Self::Output {
        Predicate::new(AndPredicateAst {
            left: self.ast.clone(),
            right: rhs.ast,
        })
    }
}

impl<'scope, L, LeftAst, R, RightAst> BitOr<Predicate<'scope, R, RightAst>>
    for Predicate<'scope, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, OrPredicate<L, R>, OrPredicateAst<LeftAst, RightAst>>;

    fn bitor(self, rhs: Predicate<'scope, R, RightAst>) -> Self::Output {
        Predicate::new(OrPredicateAst {
            left: self.ast,
            right: rhs.ast,
        })
    }
}

impl<'scope, 'rhs, L, LeftAst, R, RightAst> BitOr<&Predicate<'rhs, R, RightAst>>
    for Predicate<'scope, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, OrPredicate<L, R>, OrPredicateAst<LeftAst, RightAst>>;

    fn bitor(self, rhs: &Predicate<'rhs, R, RightAst>) -> Self::Output {
        Predicate::new(OrPredicateAst {
            left: self.ast,
            right: rhs.ast.clone(),
        })
    }
}

impl<'scope, 'lhs, L, LeftAst, R, RightAst> BitOr<Predicate<'scope, R, RightAst>>
    for &Predicate<'lhs, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, OrPredicate<L, R>, OrPredicateAst<LeftAst, RightAst>>;

    fn bitor(self, rhs: Predicate<'scope, R, RightAst>) -> Self::Output {
        Predicate::new(OrPredicateAst {
            left: self.ast.clone(),
            right: rhs.ast,
        })
    }
}

impl<'scope, K, Ast> Not for Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    type Output = Predicate<'scope, NotPredicate<K>, NotPredicateAst<Ast>>;

    fn not(self) -> Self::Output {
        Predicate::new(NotPredicateAst {
            predicate: self.ast,
        })
    }
}

impl<'scope, K, Ast> Not for &Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    type Output = Predicate<'scope, NotPredicate<K>, NotPredicateAst<Ast>>;

    fn not(self) -> Self::Output {
        Predicate::new(NotPredicateAst {
            predicate: self.ast.clone(),
        })
    }
}
