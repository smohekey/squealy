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
pub struct AggregateExprAst<Operand> {
    func: AggregateFunc,
    cast: Option<crate::SqlType>,
    operand: Operand,
}

impl<Operand> ExprAst for AggregateExprAst<Operand>
where
    Operand: ExprAst,
{
    type Params = Operand::Params;
}

impl<Operand, B> RenderAst<B> for AggregateExprAst<Operand>
where
    Operand: RenderAst<B>,
    B: crate::Backend,
{
    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor<Backend = B>,
    {
        visitor.visit_aggregate(self.func, self.cast.as_ref(), |visitor| {
            self.operand.visit(visitor)
        })
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

impl<Operand> AstProjectionClass for AggregateExprAst<Operand> {
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
/// as a single (whole-table) group. The projection and ordering must then be aggregate-only — a bare
/// column is invalid without `GROUP BY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aggregated {}

/// Validates a `SELECT`'s projection and ordering for a chain's [grouping state](Ungrouped).
///
/// An [`Ungrouped`] query must be *homogeneous* — every projected element the same class
/// ([`ProjectionClass`]) — and its ordering must match ([`OrderCompatibleWith`]), since without
/// `GROUP BY` a list cannot mix a bare column with an aggregate. An [`Aggregated`] query (a `HAVING`
/// with no `GROUP BY`) is stricter still: the projection must be aggregate-*only*, since a bare
/// column has no group to belong to. A [`Grouped`] query lifts those restrictions: a grouped list
/// may mix grouping keys and aggregates, and may order by either; the database validates that the
/// non-aggregate terms are grouping keys.
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
    Projection: ProjectionClass<Class = AggregateProjection>,
    OrderClass: OrderCompatibleWith<AggregateProjection>,
{
}

/// Maps a chain's [grouping state](Ungrouped) through a `HAVING`. A bare `HAVING` (no `GROUP BY`)
/// turns an [`Ungrouped`] chain into a whole-table [`Aggregated`] one; a chain that already has a
/// `GROUP BY` stays [`Grouped`], and an [`Aggregated`] chain stays aggregated.
#[doc(hidden)]
pub trait HavingState {
    type Output;
}

impl HavingState for Ungrouped {
    type Output = Aggregated;
}

impl HavingState for Grouped {
    type Output = Grouped;
}

impl HavingState for Aggregated {
    type Output = Aggregated;
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
        cast: Option<&crate::SqlType>,
        operand: O,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>;
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
}

macro_rules! impl_value_expr_kind {
    ($($ty:ty),* $(,)?) => {
        $(impl ExprKind for $ty {
            type Value = $ty;
        })*
    };
}

impl_value_expr_kind!(i8, i16, i32, i64, i128, isize);
impl_value_expr_kind!(u8, u16, u32, u64, u128, usize);
impl_value_expr_kind!(f32, f64);
impl_value_expr_kind!(String, bool);

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
