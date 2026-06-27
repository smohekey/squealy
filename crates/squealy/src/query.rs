use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::marker::PhantomData;
use std::pin::pin;

use futures_core::Stream;

use crate::{
    Backend, ColumnExprAst, ColumnRef, Connection, Decode, Expr, ExprAst, ExprKind, HCons, HList,
    HNil, InsertableTable, IntoNullableExprs, Maybe, NoRuntimeParams, Order, ParamExprAst,
    Predicate, PredicateKind, Projectable, ProjectionShape, PushBack, QueryBuilder, QuerySource,
    RenderAst, RenderProjectable, RuntimeParam, SourceAlias, SupportsReturning, TableProjection,
    ToTuple, UpdateableTable,
};

type ErrorOf<Builder> = <<Builder as QueryBuilder>::Backend as Backend>::Error;

/// Result of collecting every row of a query together with the affected-row count.
type RowsWithAffected<Row, Builder> = Result<(Vec<Row>, u64), ErrorOf<Builder>>;

/// Result of fetching an optional row together with the affected-row count.
type OptionalRowWithAffected<Row, Builder> = Result<(Option<Row>, u64), ErrorOf<Builder>>;

/// The row list produced by appending one `InsertRow` of `Values` to the existing `Rows`.
type PushedInsertRows<S, Columns, Rows, Values> =
    <Rows as PushBack<InsertRow<<Columns as InsertColumnValues<S, Values>>::Assignments>>>::Output;

/// Type-level identity for a table column that can be assigned in mutations.
#[doc(hidden)]
pub trait ColumnKey: ExprKind {
    type Table: TableProjection;
    type Nullability: InsertColumnNullability;

    const NAME: &'static str;
}

/// Type-level identity for a table column that may appear in explicit insert rows.
#[doc(hidden)]
pub trait InsertColumnKey: ColumnKey {}

/// Type-level identity for a table column that may appear in explicit update assignments.
#[doc(hidden)]
pub trait UpdateColumnKey: ColumnKey {}

#[doc(hidden)]
pub trait InsertColumnNullability {}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonNullableColumn {}

impl InsertColumnNullability for NonNullableColumn {}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NullableColumn {}

impl InsertColumnNullability for NullableColumn {}

/// Implemented only by [`NullableColumn`]; lets the derive gate `is_null` / `is_not_null` on a
/// column's `ColumnNullability::Nullability` where a type-level bound is workable.
#[doc(hidden)]
pub trait IsNullable {}

impl IsNullable for NullableColumn {}

/// Type-level gate for whether a column's insert-typestate slot may be left unset at `.insert()`.
/// The `Table` derive generates, per insertable column, `impl<N> InsertReady<N>` for its "set" marker
/// (always ready) and `impl InsertReady<NullableColumn>` for its "missing" marker (a nullable column
/// may be omitted). There is deliberately no `InsertReady<NonNullableColumn>` for a missing marker,
/// so omitting a required (non-null, no-default) column makes `.insert()` unavailable. `N` is the
/// column's `ColumnNullability::Nullability`.
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "this column must be set before `.insert()` (it is not nullable and has no default)"
)]
pub trait InsertReady<N> {}

#[doc(hidden)]
pub trait IntoInsertColumnValue<K, Value>
where
    K: ColumnKey,
{
    type AssignmentValue: AssignmentValueNode;

    fn into_insert_column_value(value: Value) -> Self::AssignmentValue;
}

impl<K, Value> IntoInsertColumnValue<K, Value> for NonNullableColumn
where
    K: ColumnKey,
    Value: IntoInsertAssignmentValue<K>,
{
    type AssignmentValue = <Value as IntoInsertAssignmentValue<K>>::Value;

    fn into_insert_column_value(value: Value) -> Self::AssignmentValue {
        value.into_insert_assignment_value()
    }
}

#[doc(hidden)]
pub trait IntoUpdateColumnValue<K, Value>
where
    K: ColumnKey,
{
    type AssignmentValue: AssignmentValueNode;

    fn into_update_column_value(value: Value) -> Self::AssignmentValue;
}

impl<K, Value> IntoUpdateColumnValue<K, Value> for NonNullableColumn
where
    K: ColumnKey,
    Value: IntoAssignmentValue<K>,
{
    type AssignmentValue = <Value as IntoAssignmentValue<K>>::Value;

    fn into_update_column_value(value: Value) -> Self::AssignmentValue {
        value.into_assignment_value()
    }
}

impl<K, Value> IntoUpdateColumnValue<K, Value> for NullableColumn
where
    K: ColumnKey,
    Value: IntoNullableAssignmentValue<K>,
{
    type AssignmentValue = <Value as IntoNullableAssignmentValue<K>>::Value;

    fn into_update_column_value(value: Value) -> Self::AssignmentValue {
        value.into_nullable_assignment_value()
    }
}

impl<K, Value> IntoInsertColumnValue<K, Value> for NullableColumn
where
    K: ColumnKey,
    Value: IntoNullableInsertAssignmentValue<K>,
{
    type AssignmentValue = <Value as IntoNullableInsertAssignmentValue<K>>::Value;

    fn into_insert_column_value(value: Value) -> Self::AssignmentValue {
        value.into_nullable_insert_assignment_value()
    }
}

/// Marker value for an explicit assignment that should use the database default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DefaultValueNode;

/// Use a column's database default in an explicit insert row or update assignment.
pub fn default() -> DefaultValueNode {
    DefaultValueNode
}

/// A typed insert assignment for a single generated table column.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct InsertAssignment<K, Value = DefaultAssignmentValue>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
    value: Value,
    _column: PhantomData<K>,
}

impl<K, Value> InsertAssignment<K, Value>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
    pub fn new(value: impl Into<Value>) -> Self {
        Self {
            value: value.into(),
            _column: PhantomData,
        }
    }
}

/// A typed update assignment for a single generated table column.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateAssignment<K, Value = DefaultAssignmentValue>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
    value: Value,
    _column: PhantomData<K>,
}

impl<K, Value> UpdateAssignment<K, Value>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
    pub fn new(value: impl Into<Value>) -> Self {
        Self {
            value: value.into(),
            _column: PhantomData,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct StaticAssignmentValue<T> {
    value: T,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultAssignmentValue;

#[doc(hidden)]
#[derive(Debug, PartialEq)]
pub struct ExprAssignmentValue<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    expr: Expr<'scope, K, Ast>,
}

impl<'scope, K, Ast> Clone for ExprAssignmentValue<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    fn clone(&self) -> Self {
        Self {
            expr: self.expr.clone(),
        }
    }
}

#[doc(hidden)]
#[derive(Debug, PartialEq, Eq)]
pub struct RuntimeAssignmentValue<K>
where
    K: ExprKind,
{
    _kind: PhantomData<K>,
}

#[doc(hidden)]
pub trait AssignmentValueNode: Clone {
    type Params: HList;

    fn param_count(&self) -> usize;
}

/// Backend-parameterized rendering for an assignment value (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderAssignmentValue<B>: AssignmentValueNode
where
    B: Backend,
{
    fn visit_value<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentValueVisitor<Backend = B>;
}

#[doc(hidden)]
pub trait AssignmentValueVisitor {
    type Error;
    type Backend: Backend;

    fn visit_static<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: crate::Encode<Self::Backend>;

    fn visit_default(&mut self) -> Result<(), Self::Error>;

    fn visit_runtime(&mut self) -> Result<(), Self::Error>;

    fn visit_expr<K, Ast>(&mut self, expr: &Expr<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<Self::Backend>;
}

impl<T> StaticAssignmentValue<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }
}

impl<K> RuntimeAssignmentValue<K>
where
    K: ExprKind,
{
    pub fn new() -> Self {
        Self { _kind: PhantomData }
    }
}

impl<K> Default for RuntimeAssignmentValue<K>
where
    K: ExprKind,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<'scope, K, Ast> ExprAssignmentValue<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    pub fn new(expr: Expr<'scope, K, Ast>) -> Self {
        Self { expr }
    }
}

impl<K> Clone for RuntimeAssignmentValue<K>
where
    K: ExprKind,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for RuntimeAssignmentValue<K> where K: ExprKind {}

impl<T> AssignmentValueNode for StaticAssignmentValue<T>
where
    T: Clone,
{
    type Params = HNil;

    fn param_count(&self) -> usize {
        1
    }
}

impl<T, B> RenderAssignmentValue<B> for StaticAssignmentValue<T>
where
    T: Clone + crate::Encode<B>,
    B: Backend,
{
    fn visit_value<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentValueVisitor<Backend = B>,
    {
        visitor.visit_static(&self.value)
    }
}

impl AssignmentValueNode for DefaultAssignmentValue {
    type Params = HNil;

    fn param_count(&self) -> usize {
        0
    }
}

impl<B> RenderAssignmentValue<B> for DefaultAssignmentValue
where
    B: Backend,
{
    fn visit_value<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentValueVisitor<Backend = B>,
    {
        visitor.visit_default()
    }
}

impl<K> AssignmentValueNode for RuntimeAssignmentValue<K>
where
    K: ExprKind,
{
    type Params = HCons<K::Value, HNil>;

    fn param_count(&self) -> usize {
        1
    }
}

impl<K, B> RenderAssignmentValue<B> for RuntimeAssignmentValue<K>
where
    K: ExprKind,
    B: Backend,
{
    fn visit_value<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentValueVisitor<Backend = B>,
    {
        visitor.visit_runtime()
    }
}

impl<'scope, K, Ast> AssignmentValueNode for ExprAssignmentValue<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    type Params = Ast::Params;

    fn param_count(&self) -> usize {
        count_expr_params(&self.expr)
    }
}

impl<'scope, K, Ast, B> RenderAssignmentValue<B> for ExprAssignmentValue<'scope, K, Ast>
where
    K: ExprKind,
    Ast: RenderAst<B>,
    B: Backend,
{
    fn visit_value<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentValueVisitor<Backend = B>,
    {
        visitor.visit_expr(&self.expr)
    }
}

/// Number of runtime parameters an expression contributes.
///
/// Used only as a capacity hint for param-vector preallocation, so counting just the
/// type-level runtime `Params` (literals are baked into the query, not user-supplied) is
/// sufficient and avoids a runtime AST walk.
fn count_expr_params<K, Ast>(_expr: &Expr<'_, K, Ast>) -> usize
where
    K: ExprKind,
    Ast: ExprAst,
{
    <Ast::Params as crate::HList>::LEN
}

#[doc(hidden)]
pub trait IntoAssignmentValue<K>
where
    K: ColumnKey,
{
    type Value: AssignmentValueNode;

    fn into_assignment_value(self) -> Self::Value;
}

#[doc(hidden)]
pub trait IntoNullableAssignmentValue<K>
where
    K: ColumnKey,
{
    type Value: AssignmentValueNode;

    fn into_nullable_assignment_value(self) -> Self::Value;
}

#[doc(hidden)]
pub trait IntoInsertAssignmentValue<K>
where
    K: ColumnKey,
{
    type Value: AssignmentValueNode;

    fn into_insert_assignment_value(self) -> Self::Value;
}

#[doc(hidden)]
pub trait IntoNullableInsertAssignmentValue<K>
where
    K: ColumnKey,
{
    type Value: AssignmentValueNode;

    fn into_nullable_insert_assignment_value(self) -> Self::Value;
}

impl<K, Value> IntoAssignmentValue<K> for Value
where
    K: ColumnKey,
    Value: ExprKind<Value = Value> + Clone,
{
    type Value = StaticAssignmentValue<Value>;

    fn into_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self)
    }
}

impl<K> IntoAssignmentValue<K> for &str
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue<String>;

    fn into_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.to_owned())
    }
}

impl<K> IntoAssignmentValue<K> for &String
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue<String>;

    fn into_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.clone())
    }
}

// Borrowed `bytea`/`BLOB` setters, mirroring `&str`/`&String`: `.col(&bytes)` / `.col(&bytes[..])`
// without an owned `Vec<u8>` at the call site.
impl<K> IntoAssignmentValue<K> for &[u8]
where
    K: ColumnKey<Value = Vec<u8>>,
{
    type Value = StaticAssignmentValue<Vec<u8>>;

    fn into_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.to_vec())
    }
}

impl<K> IntoAssignmentValue<K> for &Vec<u8>
where
    K: ColumnKey<Value = Vec<u8>>,
{
    type Value = StaticAssignmentValue<Vec<u8>>;

    fn into_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.clone())
    }
}

impl<K> IntoAssignmentValue<K> for DefaultValueNode
where
    K: ColumnKey,
{
    type Value = DefaultAssignmentValue;

    fn into_assignment_value(self) -> Self::Value {
        DefaultAssignmentValue
    }
}

impl<K, Value> IntoInsertAssignmentValue<K> for Value
where
    K: ColumnKey,
    Value: ExprKind<Value = Value> + Clone,
{
    type Value = StaticAssignmentValue<Value>;

    fn into_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self)
    }
}

impl<K> IntoInsertAssignmentValue<K> for &str
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue<String>;

    fn into_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.to_owned())
    }
}

impl<K> IntoInsertAssignmentValue<K> for &String
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue<String>;

    fn into_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.clone())
    }
}

impl<K> IntoInsertAssignmentValue<K> for &[u8]
where
    K: ColumnKey<Value = Vec<u8>>,
{
    type Value = StaticAssignmentValue<Vec<u8>>;

    fn into_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.to_vec())
    }
}

impl<K> IntoInsertAssignmentValue<K> for &Vec<u8>
where
    K: ColumnKey<Value = Vec<u8>>,
{
    type Value = StaticAssignmentValue<Vec<u8>>;

    fn into_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.clone())
    }
}

impl<K> IntoInsertAssignmentValue<K> for DefaultValueNode
where
    K: ColumnKey,
{
    type Value = DefaultAssignmentValue;

    fn into_insert_assignment_value(self) -> Self::Value {
        DefaultAssignmentValue
    }
}

macro_rules! impl_nullable_assignment_value {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<K> IntoNullableAssignmentValue<K> for $ty
            where
                K: ColumnKey<Value = $ty>,
            {
                type Value = StaticAssignmentValue<$ty>;

                fn into_nullable_assignment_value(self) -> Self::Value {
                    StaticAssignmentValue::new(self)
                }
            }

            impl<K> IntoNullableInsertAssignmentValue<K> for $ty
            where
                K: ColumnKey<Value = $ty>,
            {
                type Value = StaticAssignmentValue<$ty>;

                fn into_nullable_insert_assignment_value(self) -> Self::Value {
                    StaticAssignmentValue::new(self)
                }
            }
        )*
    };
}

impl_nullable_assignment_value! {
    i8, i16, i32, i64, i128, isize,
    u8, u16, u32, u64, u128, usize,
    f32, f64,
    String,
    bool,
    Vec<u8>,
}

// Native `uuid` column support: a bare `uuid::Uuid` value can be assigned to a nullable UUID column
// (`.col(id)`). `Some(id)` / `None` already route through the generic `Option<T>` impls below.
#[cfg(feature = "uuid")]
impl_nullable_assignment_value! { uuid::Uuid }

// `bytes::Bytes` column support: a bare `bytes::Bytes` value can be assigned to a nullable column
// (`.col(bytes)`); `Some`/`None` route through the generic `Option<T>` impls below.
#[cfg(feature = "bytes")]
impl_nullable_assignment_value! { bytes::Bytes }

// Native timestamp values can be assigned to a nullable timestamp column (`.col(ts)`); `Some`/`None`
// route through the generic `Option<T>` impls below.
#[cfg(feature = "systemtime")]
impl_nullable_assignment_value! { std::time::SystemTime }
#[cfg(feature = "time")]
impl_nullable_assignment_value! { time::OffsetDateTime }
#[cfg(feature = "chrono")]
impl_nullable_assignment_value! { chrono::DateTime<chrono::Utc> }

impl<K> IntoNullableAssignmentValue<K> for &str
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue<String>;

    fn into_nullable_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.to_owned())
    }
}

impl<K> IntoNullableInsertAssignmentValue<K> for &str
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue<String>;

    fn into_nullable_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.to_owned())
    }
}

impl<K> IntoNullableAssignmentValue<K> for &String
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue<String>;

    fn into_nullable_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.clone())
    }
}

impl<K> IntoNullableInsertAssignmentValue<K> for &String
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue<String>;

    fn into_nullable_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.clone())
    }
}

impl<K> IntoNullableAssignmentValue<K> for &[u8]
where
    K: ColumnKey<Value = Vec<u8>>,
{
    type Value = StaticAssignmentValue<Vec<u8>>;

    fn into_nullable_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.to_vec())
    }
}

impl<K> IntoNullableInsertAssignmentValue<K> for &[u8]
where
    K: ColumnKey<Value = Vec<u8>>,
{
    type Value = StaticAssignmentValue<Vec<u8>>;

    fn into_nullable_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.to_vec())
    }
}

impl<K> IntoNullableAssignmentValue<K> for &Vec<u8>
where
    K: ColumnKey<Value = Vec<u8>>,
{
    type Value = StaticAssignmentValue<Vec<u8>>;

    fn into_nullable_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.clone())
    }
}

impl<K> IntoNullableInsertAssignmentValue<K> for &Vec<u8>
where
    K: ColumnKey<Value = Vec<u8>>,
{
    type Value = StaticAssignmentValue<Vec<u8>>;

    fn into_nullable_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.clone())
    }
}

impl<K, T> IntoNullableAssignmentValue<K> for Option<T>
where
    K: ColumnKey<Value = T>,
    T: Clone,
{
    type Value = StaticAssignmentValue<Option<T>>;

    fn into_nullable_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self)
    }
}

impl<K, T> IntoNullableInsertAssignmentValue<K> for Option<T>
where
    K: ColumnKey<Value = T>,
    T: Clone,
{
    type Value = StaticAssignmentValue<Option<T>>;

    fn into_nullable_insert_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self)
    }
}

impl<K> IntoNullableAssignmentValue<K> for DefaultValueNode
where
    K: ColumnKey,
{
    type Value = DefaultAssignmentValue;

    fn into_nullable_assignment_value(self) -> Self::Value {
        DefaultAssignmentValue
    }
}

impl<K> IntoNullableInsertAssignmentValue<K> for DefaultValueNode
where
    K: ColumnKey,
{
    type Value = DefaultAssignmentValue;

    fn into_nullable_insert_assignment_value(self) -> Self::Value {
        DefaultAssignmentValue
    }
}

impl<'scope, K> IntoInsertAssignmentValue<K> for Expr<'scope, RuntimeParam<K>, ParamExprAst<K>>
where
    K: ColumnKey,
{
    type Value = RuntimeAssignmentValue<K>;

    fn into_insert_assignment_value(self) -> Self::Value {
        RuntimeAssignmentValue::new()
    }
}

impl<'scope, K> IntoNullableInsertAssignmentValue<K>
    for Expr<'scope, RuntimeParam<K>, ParamExprAst<K>>
where
    K: ColumnKey,
{
    type Value = RuntimeAssignmentValue<K>;

    fn into_nullable_insert_assignment_value(self) -> Self::Value {
        RuntimeAssignmentValue::new()
    }
}

impl<'scope, K, ExprK, Ast> IntoAssignmentValue<K> for Expr<'scope, ExprK, Ast>
where
    K: ColumnKey,
    ExprK: ExprKind<Value = K::Value>,
    // Aggregates are invalid in an `UPDATE ... SET` value (`SET x = COUNT(...)`), so the assignment
    // expression must be aggregate-free.
    Ast: ExprAst + crate::NonAggregateAst,
{
    type Value = ExprAssignmentValue<'scope, ExprK, Ast>;

    fn into_assignment_value(self) -> Self::Value {
        ExprAssignmentValue::new(self)
    }
}

impl<'scope, K, ExprK> IntoAssignmentValue<K> for ColumnRef<'scope, ExprK>
where
    K: ColumnKey,
    ExprK: ExprKind<Value = K::Value>,
{
    type Value = ExprAssignmentValue<'scope, ExprK, ColumnExprAst<ExprK>>;

    fn into_assignment_value(self) -> Self::Value {
        ExprAssignmentValue::new(self.into_expr())
    }
}

impl<'scope, K, ExprK, Ast> IntoNullableAssignmentValue<K> for Expr<'scope, ExprK, Ast>
where
    K: ColumnKey,
    ExprK: ExprKind<Value = K::Value>,
    // As with the non-null case, aggregates are invalid in an `UPDATE ... SET` value.
    Ast: ExprAst + crate::NonAggregateAst,
{
    type Value = ExprAssignmentValue<'scope, ExprK, Ast>;

    fn into_nullable_assignment_value(self) -> Self::Value {
        ExprAssignmentValue::new(self)
    }
}

impl<'scope, K, ExprK> IntoNullableAssignmentValue<K> for ColumnRef<'scope, ExprK>
where
    K: ColumnKey,
    ExprK: ExprKind<Value = K::Value>,
{
    type Value = ExprAssignmentValue<'scope, ExprK, ColumnExprAst<ExprK>>;

    fn into_nullable_assignment_value(self) -> Self::Value {
        ExprAssignmentValue::new(self.into_expr())
    }
}

#[doc(hidden)]
pub trait AssignmentNode {
    type Params: HList;

    fn column(&self) -> &'static str;

    fn param_count(&self) -> usize;
}

/// Backend-parameterized rendering for an assignment (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderAssignment<B>: AssignmentNode
where
    B: Backend,
{
    fn visit_value<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentValueVisitor<Backend = B>;
}

#[doc(hidden)]
pub trait InsertAssignmentNode: AssignmentNode {}

impl<K, Value> AssignmentNode for InsertAssignment<K, Value>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
    type Params = Value::Params;

    fn column(&self) -> &'static str {
        K::NAME
    }

    fn param_count(&self) -> usize {
        self.value.param_count()
    }
}

impl<K, Value, B> RenderAssignment<B> for InsertAssignment<K, Value>
where
    K: ColumnKey,
    Value: RenderAssignmentValue<B>,
    B: Backend,
{
    fn visit_value<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentValueVisitor<Backend = B>,
    {
        self.value.visit_value(visitor)
    }
}

impl<K, Value> InsertAssignmentNode for InsertAssignment<K, Value>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
}

#[doc(hidden)]
pub trait UpdateAssignmentNode: AssignmentNode {}

impl<K, Value> AssignmentNode for UpdateAssignment<K, Value>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
    type Params = Value::Params;

    fn column(&self) -> &'static str {
        K::NAME
    }

    fn param_count(&self) -> usize {
        self.value.param_count()
    }
}

impl<K, Value, B> RenderAssignment<B> for UpdateAssignment<K, Value>
where
    K: ColumnKey,
    Value: RenderAssignmentValue<B>,
    B: Backend,
{
    fn visit_value<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentValueVisitor<Backend = B>,
    {
        self.value.visit_value(visitor)
    }
}

impl<K, Value> UpdateAssignmentNode for UpdateAssignment<K, Value>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
}

#[doc(hidden)]
pub trait AssignmentVisitor {
    type Error;
    type Backend: Backend;

    fn visit_assignment<Value>(
        &mut self,
        column: &'static str,
        value: &Value,
    ) -> Result<(), Self::Error>
    where
        Value: RenderAssignment<Self::Backend>;
}

/// A typed insert row containing the assignments for one SQL `VALUES` group.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct InsertRow<Columns>
where
    Columns: InsertAssignments,
{
    columns: Columns,
}

impl<Columns> InsertRow<Columns>
where
    Columns: InsertAssignments,
{
    pub fn new(columns: Columns) -> Self {
        Self { columns }
    }

    pub fn columns(&self) -> &Columns {
        &self.columns
    }
}

/// Visitor used to traverse heterogeneously typed insert rows without allocation.
#[doc(hidden)]
pub trait InsertRowVisitor<E> {
    type Backend: Backend;

    fn visit_row<Columns>(&mut self, row: &InsertRow<Columns>) -> Result<(), E>
    where
        Columns: RenderInsertAssignments<Self::Backend>;
}

/// Heterogeneous list of typed insert rows.
#[doc(hidden)]
pub trait InsertRows {
    type Params: HList;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn first_row_len(&self) -> usize;

    fn try_for_each_column<E>(&self, f: impl FnMut(&'static str) -> Result<(), E>)
    -> Result<(), E>;

    fn param_count(&self) -> usize;
}

/// Backend-parameterized row traversal for insert rows (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderInsertRows<B>: InsertRows
where
    B: Backend,
{
    fn try_for_each_row<E, Visitor>(&self, visitor: &mut Visitor) -> Result<(), E>
    where
        Visitor: InsertRowVisitor<E, Backend = B>;
}

#[doc(hidden)]
pub trait NonEmptyInsertRows: InsertRows {}

impl InsertRows for HNil {
    type Params = HNil;

    fn len(&self) -> usize {
        0
    }

    fn first_row_len(&self) -> usize {
        0
    }

    fn try_for_each_column<E>(
        &self,
        _f: impl FnMut(&'static str) -> Result<(), E>,
    ) -> Result<(), E> {
        Ok(())
    }

    fn param_count(&self) -> usize {
        0
    }
}

impl<B> RenderInsertRows<B> for HNil
where
    B: Backend,
{
    fn try_for_each_row<E, Visitor>(&self, _visitor: &mut Visitor) -> Result<(), E>
    where
        Visitor: InsertRowVisitor<E, Backend = B>,
    {
        Ok(())
    }
}

impl<Columns, Tail> InsertRows for HCons<InsertRow<Columns>, Tail>
where
    Columns: InsertAssignments,
    Tail: InsertRows,
    Columns::Params: crate::HAppend<Tail::Params>,
{
    type Params = <Columns::Params as crate::HAppend<Tail::Params>>::Output;

    fn len(&self) -> usize {
        1 + self.tail.len()
    }

    fn first_row_len(&self) -> usize {
        self.head.columns().len()
    }

    fn try_for_each_column<E>(
        &self,
        f: impl FnMut(&'static str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.head.columns().try_for_each_column(f)
    }

    fn param_count(&self) -> usize {
        self.head.columns().param_count() + self.tail.param_count()
    }
}

impl<Columns, Tail, B> RenderInsertRows<B> for HCons<InsertRow<Columns>, Tail>
where
    Columns: RenderInsertAssignments<B>,
    Tail: RenderInsertRows<B>,
    Columns::Params: crate::HAppend<Tail::Params>,
    B: Backend,
{
    fn try_for_each_row<E, Visitor>(&self, visitor: &mut Visitor) -> Result<(), E>
    where
        Visitor: InsertRowVisitor<E, Backend = B>,
    {
        visitor.visit_row(&self.head)?;
        self.tail.try_for_each_row(visitor)
    }
}

impl<Columns, Tail> NonEmptyInsertRows for HCons<InsertRow<Columns>, Tail>
where
    Columns: InsertAssignments,
    Tail: InsertRows,
    Columns::Params: crate::HAppend<Tail::Params>,
{
}

/// Converts a tuple of row values into a typed insert assignment list for a fixed column tuple.
#[doc(hidden)]
pub trait InsertColumnValues<S, Values>
where
    S: InsertableTable,
{
    type Assignments: InsertAssignments;

    fn into_insert_assignments(values: Values) -> Self::Assignments;
}

impl<S> InsertColumnValues<S, ()> for ()
where
    S: InsertableTable,
{
    type Assignments = HNil;

    fn into_insert_assignments(_values: ()) -> Self::Assignments {
        HNil
    }
}

squealy_macros::insert_column_values!(32);

/// Converts a tuple of update values into a typed update assignment list for a fixed column tuple.
#[doc(hidden)]
pub trait UpdateColumnValues<S, Values>
where
    S: UpdateableTable,
{
    type Assignments: UpdateAssignments;

    fn into_update_assignments(values: Values) -> Self::Assignments;
}

impl<S> UpdateColumnValues<S, ()> for ()
where
    S: UpdateableTable,
{
    type Assignments = HNil;

    fn into_update_assignments(_values: ()) -> Self::Assignments {
        HNil
    }
}

squealy_macros::update_column_values!(32);

/// Heterogeneous list of typed insert assignments.
#[doc(hidden)]
pub trait InsertAssignments {
    type Params: HList;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn try_for_each_column<E>(&self, f: impl FnMut(&'static str) -> Result<(), E>)
    -> Result<(), E>;

    fn param_count(&self) -> usize;
}

/// Backend-parameterized traversal for insert assignments (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderInsertAssignments<B>: InsertAssignments
where
    B: Backend,
{
    fn try_visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentVisitor<Backend = B>;
}

impl InsertAssignments for HNil {
    type Params = HNil;

    fn len(&self) -> usize {
        0
    }

    fn try_for_each_column<E>(
        &self,
        _f: impl FnMut(&'static str) -> Result<(), E>,
    ) -> Result<(), E> {
        Ok(())
    }

    fn param_count(&self) -> usize {
        0
    }
}

impl<B> RenderInsertAssignments<B> for HNil
where
    B: Backend,
{
    fn try_visit<V>(&self, _visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentVisitor<Backend = B>,
    {
        Ok(())
    }
}

impl<Head, Tail> InsertAssignments for HCons<Head, Tail>
where
    Head: InsertAssignmentNode,
    Tail: InsertAssignments,
    Head::Params: crate::HAppend<Tail::Params>,
{
    type Params = <Head::Params as crate::HAppend<Tail::Params>>::Output;

    fn len(&self) -> usize {
        1 + self.tail.len()
    }

    fn try_for_each_column<E>(
        &self,
        mut f: impl FnMut(&'static str) -> Result<(), E>,
    ) -> Result<(), E> {
        f(self.head.column())?;
        self.tail.try_for_each_column(f)
    }

    fn param_count(&self) -> usize {
        self.head.param_count() + self.tail.param_count()
    }
}

impl<Head, Tail, B> RenderInsertAssignments<B> for HCons<Head, Tail>
where
    Head: InsertAssignmentNode + RenderAssignment<B>,
    Tail: RenderInsertAssignments<B>,
    Head::Params: crate::HAppend<Tail::Params>,
    B: Backend,
{
    fn try_visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentVisitor<Backend = B>,
    {
        visitor.visit_assignment(self.head.column(), &self.head)?;
        self.tail.try_visit(visitor)
    }
}

/// Heterogeneous list of typed update assignments.
#[doc(hidden)]
pub trait UpdateAssignments {
    type Params: HList;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn param_count(&self) -> usize;
}

/// Backend-parameterized traversal for update assignments (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderUpdateAssignments<B>: UpdateAssignments
where
    B: Backend,
{
    fn try_visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentVisitor<Backend = B>;
}

impl UpdateAssignments for HNil {
    type Params = HNil;

    fn len(&self) -> usize {
        0
    }

    fn param_count(&self) -> usize {
        0
    }
}

impl<B> RenderUpdateAssignments<B> for HNil
where
    B: Backend,
{
    fn try_visit<V>(&self, _visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentVisitor<Backend = B>,
    {
        Ok(())
    }
}

impl<Head, Tail> UpdateAssignments for HCons<Head, Tail>
where
    Head: UpdateAssignmentNode,
    Tail: UpdateAssignments,
    Head::Params: crate::HAppend<Tail::Params>,
{
    type Params = <Head::Params as crate::HAppend<Tail::Params>>::Output;

    fn len(&self) -> usize {
        1 + self.tail.len()
    }

    fn param_count(&self) -> usize {
        self.head.param_count() + self.tail.param_count()
    }
}

impl<Head, Tail, B> RenderUpdateAssignments<B> for HCons<Head, Tail>
where
    Head: UpdateAssignmentNode + RenderAssignment<B>,
    Tail: RenderUpdateAssignments<B>,
    Head::Params: crate::HAppend<Tail::Params>,
    B: Backend,
{
    fn try_visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: AssignmentVisitor<Backend = B>,
    {
        visitor.visit_assignment(self.head.column(), &self.head)?;
        self.tail.try_visit(visitor)
    }
}

/// Heterogeneous list of typed predicates.
#[doc(hidden)]
pub trait PredicateNodes {
    type Params: HList;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Backend-parameterized rendering for a predicate-node list (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderPredicateNodes<B>: PredicateNodes
where
    B: Backend,
{
    fn try_visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateVisitor<Backend = B>;
}

#[doc(hidden)]
pub trait PredicateVisitor {
    type Error;
    type Backend: Backend;

    fn visit_predicate<Kind, Ast>(
        &mut self,
        predicate: &Predicate<'_, Kind, Ast>,
    ) -> Result<(), Self::Error>
    where
        Kind: PredicateKind,
        Ast: crate::RenderPredicateAst<Self::Backend>;
}

impl PredicateNodes for HNil {
    type Params = HNil;

    fn len(&self) -> usize {
        0
    }
}

impl<B> RenderPredicateNodes<B> for HNil
where
    B: Backend,
{
    fn try_visit<V>(&self, _visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateVisitor<Backend = B>,
    {
        Ok(())
    }
}

impl<'scope, Kind, Ast, Tail> PredicateNodes for HCons<Predicate<'scope, Kind, Ast>, Tail>
where
    Kind: PredicateKind,
    Ast: crate::PredicateAst,
    Tail: PredicateNodes,
    Ast::Params: crate::HAppend<Tail::Params>,
{
    type Params = <Ast::Params as crate::HAppend<Tail::Params>>::Output;

    fn len(&self) -> usize {
        1 + self.tail.len()
    }
}

impl<'scope, Kind, Ast, Tail, B> RenderPredicateNodes<B>
    for HCons<Predicate<'scope, Kind, Ast>, Tail>
where
    Kind: PredicateKind,
    Ast: crate::RenderPredicateAst<B>,
    Tail: RenderPredicateNodes<B>,
    Ast::Params: crate::HAppend<Tail::Params>,
    B: Backend,
{
    fn try_visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateVisitor<Backend = B>,
    {
        visitor.visit_predicate(&self.head)?;
        self.tail.try_visit(visitor)
    }
}

/// A row stream that can report affected rows after it is exhausted.
pub trait RowsAffected {
    fn rows_affected(&self) -> Option<u64>;
}

/// A backend-owned prepared select statement.
///
/// Prepared statements are bound to the connection/backend that produced them. `Params` is the
/// typed runtime parameter shape accepted by each execution. For concrete queries whose bind values
/// are already stored in the query object, this can be `()`.
pub trait PreparedSelectQuery<'conn> {
    type Builder: Connection + 'conn;
    type Params: HList;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    type RowStream<'query>: Stream<Item = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        Self: 'query;

    fn fetch<'query, ParamValues>(&'query self, params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues:
            crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>;

    fn collect<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<Vec<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let rows = self.fetch(params);
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let row = fetch_optional_row::<Self::Builder, Self::Row, _>(self.fetch(params));
        async move {
            row.await?
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)
        }
    }

    fn fetch_optional<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<Option<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let rows = self.fetch(params);
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// A backend-owned prepared insert, update, or delete statement.
///
/// Prepared mutation statements can either execute for affected-row counts or fetch rows when the
/// mutation has a returning projection.
pub trait PreparedMutationQuery<'conn> {
    type Builder: Connection + 'conn;
    type Params: HList;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    type RowStream<'query>: Stream<Item = Result<Self::Row, ErrorOf<Self::Builder>>>
        + Send
        + RowsAffected
        + 'query
    where
        Self: 'query;

    fn execute<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<u64, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query;

    fn fetch<'query, ParamValues>(&'query self, params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues:
            crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>;

    fn collect<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<Vec<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let rows = self.fetch(params);
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = RowsWithAffected<Self::Row, Self::Builder>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let rows = self.fetch(params);
        collect_rows_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one_with_affected<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<(Self::Row, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let row =
            fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(self.fetch(params));
        async move {
            let (row, affected) = row.await?;
            let row = row
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)?;
            Ok((row, affected))
        }
    }

    fn fetch_optional_with_affected<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = OptionalRowWithAffected<Self::Row, Self::Builder>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let rows = self.fetch(params);
        fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let row = fetch_optional_row::<Self::Builder, Self::Row, _>(self.fetch(params));
        async move {
            row.await?
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)
        }
    }

    fn fetch_optional<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<Option<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params, <Self::Builder as QueryBuilder>::Backend>
            + 'query,
    {
        let rows = self.fetch(params);
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// A backend-specific select query object backed by core-owned select typestates.
pub trait SelectQuery<'builder, 'scope, Base, Projection>
where
    Base: SelectAst<'builder, 'scope, Self::Builder>,
    Projection: Projectable,
{
    type Builder: QueryBuilder + 'builder;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn build_selected(
        builder: &'builder Self::Builder,
        selected: Selected<'scope, Base, Self::Shape, Projection>,
    ) -> Self
    where
        Self: Sized;
}

/// A select query object that can fetch rows through an executable connection.
pub trait ExecutableSelectQuery<'conn, 'scope, Base, Projection>:
    SelectQuery<'conn, 'scope, Base, Projection>
where
    Self::Builder: Connection,
    Base: SelectAst<'conn, 'scope, Self::Builder>,
    Base::Params: NoRuntimeParams,
    Projection: Projectable,
{
    type RowStream<'query>: Stream<Item = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        Self: 'query;

    fn fetch<'query>(&'query self) -> Self::RowStream<'query>;

    fn collect<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        'scope: 'query,
        Base: 'query,
        Projection: 'query,
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        'scope: 'query,
        Base: 'query,
        Projection: 'query,
    {
        let row = fetch_optional_row::<Self::Builder, Self::Row, _>(self.fetch());
        async move {
            row.await?
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)
        }
    }

    fn fetch_optional<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Option<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        'scope: 'query,
        Base: 'query,
        Projection: 'query,
    {
        let rows = self.fetch();
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// A select query object that can be compiled into a backend-owned prepared statement.
pub trait PreparableSelectQuery<'conn, 'scope, Base, Projection>:
    SelectQuery<'conn, 'scope, Base, Projection>
where
    Self::Builder: Connection,
    Base: SelectAst<'conn, 'scope, Self::Builder>,
    Projection: Projectable,
{
    type Params: HList;

    type Prepared<'prepared>: PreparedSelectQuery<
            'prepared,
            Builder = Self::Builder,
            Params = Self::Params,
            Row = Self::Row,
        > + 'prepared
    where
        Self: 'prepared,
        'conn: 'prepared,
        'scope: 'prepared,
        Base: 'prepared,
        Projection: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<Output = Result<Self::Prepared<'prepared>, ErrorOf<Self::Builder>>> + 'prepared
    where
        'conn: 'prepared,
        'scope: 'prepared,
        Base: 'prepared,
        Projection: 'prepared;
}

/// A backend-specific insert query object built from typed insert state.
/// The `ON CONFLICT` clause of an upsert (PostgreSQL). Carried as a runtime value on the insert query
/// — `do_nothing`/`do_update`(replace-all) add no bind parameters, so no type-level plumbing is needed.
#[derive(Clone, Debug)]
pub struct ConflictClause {
    /// The conflict-target columns, rendered as `ON CONFLICT (<cols>)`.
    pub target: Vec<&'static str>,
    pub action: ConflictAction,
}

/// What to do on an `ON CONFLICT` match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictAction {
    /// `DO NOTHING`.
    DoNothing,
    /// `DO UPDATE SET <col> = EXCLUDED.<col>` for every inserted column (replace with the proposed row).
    DoUpdateExcluded,
}

/// The conflict target of an upsert — the column(s) in `ON CONFLICT (<cols>)`. Implemented for a single
/// column reference and tuples of column references, so `on_conflict(|t| t.email)` and
/// `on_conflict(|t| (t.a, t.b))` both type-check against the table's columns.
pub trait ConflictTarget {
    /// The conflict-target column names, in order.
    fn column_names(self) -> Vec<&'static str>;
}

impl<'scope, K> ConflictTarget for crate::ColumnRef<'scope, K>
where
    K: ExprKind,
{
    fn column_names(self) -> Vec<&'static str> {
        vec![self.column_name()]
    }
}

macro_rules! impl_conflict_target_tuple {
    ($($name:ident),+) => {
        impl<$($name: ConflictTarget),+> ConflictTarget for ($($name,)+) {
            fn column_names(self) -> Vec<&'static str> {
                #[allow(non_snake_case)]
                let ($($name,)+) = self;
                let mut names = Vec::new();
                $(names.extend($name.column_names());)+
                names
            }
        }
    };
}
impl_conflict_target_tuple!(A);
impl_conflict_target_tuple!(A, B);
impl_conflict_target_tuple!(A, B, C);
impl_conflict_target_tuple!(A, B, C, D);

/// A query builder whose backend supports `INSERT … ON CONFLICT` (PostgreSQL). Gating `on_conflict` on
/// this keeps upsert off backends that don't render it (e.g. MySQL's `ON DUPLICATE KEY UPDATE` is a
/// later follow-up). The upsert reuses the existing `Insert` query object (the conflict clause is a
/// runtime field), so this just constructs it with the clause attached.
pub trait OnConflictQueryBuilder: QueryBuilder {
    fn build_upsert<'conn, S, Shape, Rows, Returning>(
        &'conn self,
        rows: Rows,
        returning: Returning,
        conflict: ConflictClause,
    ) -> Self::Insert<'conn, S, Shape, Rows, Returning>
    where
        Self: 'conn,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Rows: InsertRows,
        Returning: Projectable;
}

/// Upsert builder produced by `on_conflict(target)` — choose `do_nothing()` or `do_update()`.
#[doc(hidden)]
pub struct OnConflict<'conn, Conn, S, InsertColumns> {
    connection: &'conn Conn,
    insert_columns: InsertColumns,
    target: Vec<&'static str>,
    _table: PhantomData<S>,
}

impl<'conn, Conn, S, InsertColumns> OnConflict<'conn, Conn, S, InsertColumns> {
    #[doc(hidden)]
    pub fn new(
        connection: &'conn Conn,
        insert_columns: InsertColumns,
        target: Vec<&'static str>,
    ) -> Self {
        Self {
            connection,
            insert_columns,
            target,
            _table: PhantomData,
        }
    }

    /// `ON CONFLICT (<target>) DO NOTHING`.
    pub fn do_nothing(self) -> Upsert<'conn, Conn, S, InsertColumns> {
        Upsert::new(
            self.connection,
            self.insert_columns,
            ConflictClause {
                target: self.target,
                action: ConflictAction::DoNothing,
            },
        )
    }

    /// `ON CONFLICT (<target>) DO UPDATE SET <col> = EXCLUDED.<col>` for every inserted column (replace
    /// the existing row with the values being inserted).
    pub fn do_update(self) -> Upsert<'conn, Conn, S, InsertColumns> {
        Upsert::new(
            self.connection,
            self.insert_columns,
            ConflictClause {
                target: self.target,
                action: ConflictAction::DoUpdateExcluded,
            },
        )
    }
}

/// A finalized upsert — call `insert()` or `insert_returning(|row| …)`.
#[doc(hidden)]
pub struct Upsert<'conn, Conn, S, InsertColumns> {
    connection: &'conn Conn,
    insert_columns: InsertColumns,
    conflict: ConflictClause,
    _table: PhantomData<S>,
}

impl<'conn, Conn, S, InsertColumns> Upsert<'conn, Conn, S, InsertColumns> {
    fn new(
        connection: &'conn Conn,
        insert_columns: InsertColumns,
        conflict: ConflictClause,
    ) -> Self {
        Self {
            connection,
            insert_columns,
            conflict,
            _table: PhantomData,
        }
    }

    pub fn insert(self) -> impl Future<Output = Result<u64, ErrorOf<Conn>>> + Send + 'conn
    where
        Conn: Connection + OnConflictQueryBuilder + 'conn,
        S: InsertableTable + 'conn,
        InsertColumns: InsertAssignments + 'conn,
        HCons<InsertRow<InsertColumns>, HNil>: InsertRows,
        <HCons<InsertRow<InsertColumns>, HNil> as InsertRows>::Params: NoRuntimeParams,
        Conn::Insert<'conn, S, (), HCons<InsertRow<InsertColumns>, HNil>, ()>:
            ExecutableInsertQuery<'conn, HCons<InsertRow<InsertColumns>, HNil>, ()> + Send,
    {
        let rows = HCons {
            head: InsertRow::new(self.insert_columns),
            tail: HNil,
        };
        let query = self
            .connection
            .build_upsert::<S, (), _, ()>(rows, (), self.conflict);
        async move { ExecutableInsertQuery::execute(&query).await }
    }

    pub fn insert_returning<P>(
        self,
        projection: impl FnOnce(<S as ProjectionShape>::Exprs<'static>) -> P,
    ) -> Conn::Insert<
        'conn,
        S,
        <P as ReturningProjection<'static>>::Shape,
        HCons<InsertRow<InsertColumns>, HNil>,
        P,
    >
    where
        Conn: OnConflictQueryBuilder + 'conn,
        S: InsertableTable + ProjectionShape + 'conn,
        InsertColumns: InsertAssignments + 'conn,
        HCons<InsertRow<InsertColumns>, HNil>: InsertRows,
        P: ReturningProjection<'static>
            + Projectable
            + crate::ProjectionClass<Class = crate::ScalarProjection>
            + crate::ReturnableProjection
            + crate::ProjectionParams<Params = HNil>,
        <P::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
        Conn::Backend: SupportsReturning,
    {
        let rows = HCons {
            head: InsertRow::new(self.insert_columns),
            tail: HNil,
        };
        let table = <S as ProjectionShape>::exprs(SourceAlias::new(0, 0));
        let projection = projection(table);
        self.connection
            .build_upsert::<S, <P as ReturningProjection<'static>>::Shape, _, P>(
                rows,
                projection,
                self.conflict,
            )
    }
}

pub trait InsertQuery<'builder, Rows, Returning>
where
    Rows: InsertRows,
    Returning: Projectable,
{
    type Builder: QueryBuilder + 'builder;
    type Table: InsertableTable;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn build(builder: &'builder Self::Builder, rows: Rows, returning: Returning) -> Self
    where
        Self: Sized;
}

/// An insert query object that can execute or fetch rows through a connection.
pub trait ExecutableInsertQuery<'conn, Rows, Returning>:
    InsertQuery<'conn, Rows, Returning>
where
    Self::Builder: Connection,
    Rows: InsertRows,
    Rows::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>: Stream<Item = Result<Self::Row, ErrorOf<Self::Builder>>>
        + Send
        + RowsAffected
        + 'query
    where
        Self: 'query;

    fn execute(&self) -> impl Future<Output = Result<u64, ErrorOf<Self::Builder>>> + Send + '_;

    fn fetch<'query>(&'query self) -> Self::RowStream<'query>;

    fn collect<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Rows: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = RowsWithAffected<Self::Row, Self::Builder>> + Send + 'query
    where
        'conn: 'query,
        Rows: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        collect_rows_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Self::Row, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Rows: 'query,
        Returning: 'query,
    {
        let row = fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(self.fetch());
        async move {
            let (row, affected) = row.await?;
            let row = row
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)?;
            Ok((row, affected))
        }
    }

    fn fetch_optional_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = OptionalRowWithAffected<Self::Row, Self::Builder>> + Send + 'query
    where
        'conn: 'query,
        Rows: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Rows: 'query,
        Returning: 'query,
    {
        let row = fetch_optional_row::<Self::Builder, Self::Row, _>(self.fetch());
        async move {
            row.await?
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)
        }
    }

    fn fetch_optional<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Option<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Rows: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// An insert query object that can be compiled into a backend-owned prepared statement.
pub trait PreparableInsertQuery<'conn, Rows, Returning>:
    InsertQuery<'conn, Rows, Returning>
where
    Self::Builder: Connection,
    Rows: InsertRows,
    Returning: Projectable,
{
    type Params: HList;

    type Prepared<'prepared>: PreparedMutationQuery<
            'prepared,
            Builder = Self::Builder,
            Params = Self::Params,
            Row = Self::Row,
        > + 'prepared
    where
        Self: 'prepared,
        'conn: 'prepared,
        Rows: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<Output = Result<Self::Prepared<'prepared>, ErrorOf<Self::Builder>>> + 'prepared
    where
        'conn: 'prepared,
        Rows: 'prepared,
        Returning: 'prepared;
}

/// Mutation builder for an explicit table column tuple.
pub struct ToColumns<'conn, Conn, S, Columns, Rows = HNil>
where
    Conn: QueryBuilder + 'conn,
    Rows: InsertRows,
{
    connection: &'conn Conn,
    rows: Rows,
    _table: PhantomData<S>,
    _columns: PhantomData<Columns>,
}

/// Backwards-compatible name for the explicit insert rows builder.
#[doc(hidden)]
pub type InsertRowsBuilder<'conn, Conn, S, Columns, Rows = HNil> =
    ToColumns<'conn, Conn, S, Columns, Rows>;

impl<'conn, Conn, S, Columns> ToColumns<'conn, Conn, S, Columns, HNil>
where
    Conn: QueryBuilder + 'conn,
{
    pub(crate) fn new(connection: &'conn Conn) -> Self {
        Self {
            connection,
            rows: HNil,
            _table: PhantomData,
            _columns: PhantomData,
        }
    }
}

impl<'conn, Conn, S, Columns, Rows> ToColumns<'conn, Conn, S, Columns, Rows>
where
    Conn: QueryBuilder + 'conn,
    S: InsertableTable,
    Rows: InsertRows,
{
    pub fn row<Values>(
        self,
        values: Values,
    ) -> ToColumns<'conn, Conn, S, Columns, PushedInsertRows<S, Columns, Rows, Values>>
    where
        Columns: InsertColumnValues<S, Values>,
        Rows: PushBack<InsertRow<<Columns as InsertColumnValues<S, Values>>::Assignments>>,
        <Rows as PushBack<InsertRow<<Columns as InsertColumnValues<S, Values>>::Assignments>>>::Output:
            InsertRows,
    {
        let row = InsertRow::new(Columns::into_insert_assignments(values));
        ToColumns {
            connection: self.connection,
            rows: self.rows.push_back(row),
            _table: PhantomData,
            _columns: PhantomData,
        }
    }

    pub fn insert(self) -> impl Future<Output = Result<u64, ErrorOf<Conn>>> + Send + 'conn
    where
        Conn: Connection + 'conn,
        S: 'conn,
        Rows: NonEmptyInsertRows + 'conn,
        Rows::Params: NoRuntimeParams,
        // The returned future captures the built query object, so it must be `Send` for the future
        // to be `Send` behind a generic `async fn -> impl Future + Send` trait method. This proves
        // `Send` directly instead of leaking it through the (lifetime-specific) execution impl.
        <Conn as QueryBuilder>::Insert<'conn, S, (), Rows, ()>:
            ExecutableInsertQuery<'conn, Rows, ()> + Send,
    {
        let query = <<Conn as QueryBuilder>::Insert<'conn, S, (), Rows, ()> as InsertQuery<
            'conn,
            Rows,
            (),
        >>::build(self.connection, self.rows, ());
        async move { ExecutableInsertQuery::execute(&query).await }
    }

    pub fn insert_returning<P>(
        self,
        projection: impl FnOnce(<S as ProjectionShape>::Exprs<'static>) -> P,
    ) -> Conn::Insert<'conn, S, <P as ReturningProjection<'static>>::Shape, Rows, P>
    where
        S: ProjectionShape + 'conn,
        Rows: NonEmptyInsertRows + 'conn,
        // Aggregates are never valid in `RETURNING`, so require an aggregate-free projection.
        P: ReturningProjection<'static>
            + Projectable
            + crate::ProjectionClass<Class = crate::ScalarProjection>
            // Window functions are invalid in a RETURNING clause; `ReturnableProjection` excludes them
            + crate::ReturnableProjection
            + crate::ProjectionParams<Params = HNil>,
        <P::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
        Conn::Backend: SupportsReturning,
    {
        let table = <S as ProjectionShape>::exprs(SourceAlias::new(0, 0));
        let projection = projection(table);
        <<Conn as QueryBuilder>::Insert<
            'conn,
            S,
            <P as ReturningProjection<'static>>::Shape,
            Rows,
            P,
        > as InsertQuery<'conn, Rows, P>>::build(self.connection, self.rows, projection)
    }
}

impl<'conn, Conn, S, Columns, Rows> ToColumns<'conn, Conn, S, Columns, Rows>
where
    Conn: QueryBuilder + 'conn,
    S: UpdateableTable + ProjectionShape,
    Rows: InsertRows,
{
    pub fn set<Values>(
        self,
        values: impl FnOnce(<S as ProjectionShape>::Exprs<'static>) -> Values,
    ) -> ExplicitUpdateBuilder<
        'conn,
        Conn,
        S,
        <Columns as UpdateColumnValues<S, Values>>::Assignments,
    >
    where
        Columns: UpdateColumnValues<S, Values>,
    {
        let alias = SourceAlias::new(0, 0);
        let table = <S as ProjectionShape>::exprs(alias);
        ExplicitUpdateBuilder {
            connection: self.connection,
            alias,
            columns: Columns::into_update_assignments(values(table)),
            filters: HNil,
            _table: PhantomData,
            _state: PhantomData,
        }
    }
}

pub struct ExplicitUpdateBuilder<
    'conn,
    Conn,
    S,
    Columns,
    Filters = HNil,
    FilterState = MutationUnfiltered,
> where
    Conn: QueryBuilder + 'conn,
    S: UpdateableTable,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
{
    connection: &'conn Conn,
    alias: SourceAlias,
    columns: Columns,
    filters: Filters,
    _table: PhantomData<S>,
    _state: PhantomData<FilterState>,
}

impl<'conn, Conn, S, Columns, Filters, FilterState>
    ExplicitUpdateBuilder<'conn, Conn, S, Columns, Filters, FilterState>
where
    Conn: QueryBuilder + 'conn,
    S: UpdateableTable + ProjectionShape,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
{
    pub fn where_<P, PredicateAst>(
        self,
        predicate: impl FnOnce(
            <S as ProjectionShape>::Exprs<'static>,
        ) -> Predicate<'static, P, PredicateAst>,
    ) -> ExplicitUpdateBuilder<
        'conn,
        Conn,
        S,
        Columns,
        <Filters as PushBack<Predicate<'static, P, PredicateAst>>>::Output,
        MutationFiltered,
    >
    where
        P: PredicateKind,
        PredicateAst: crate::PredicateAst + crate::NonAggregatePredicate,
        Filters: PushBack<Predicate<'static, P, PredicateAst>>,
        <Filters as PushBack<Predicate<'static, P, PredicateAst>>>::Output: PredicateNodes,
    {
        let table = <S as ProjectionShape>::exprs(self.alias);
        ExplicitUpdateBuilder {
            connection: self.connection,
            alias: self.alias,
            columns: self.columns,
            filters: self.filters.push_back(predicate(table)),
            _table: PhantomData,
            _state: PhantomData,
        }
    }

    pub fn all(self) -> ExplicitUpdateBuilder<'conn, Conn, S, Columns, Filters, MutationFiltered> {
        ExplicitUpdateBuilder {
            connection: self.connection,
            alias: self.alias,
            columns: self.columns,
            filters: self.filters,
            _table: PhantomData,
            _state: PhantomData,
        }
    }
}

impl<'conn, Conn, S, Columns, Filters>
    ExplicitUpdateBuilder<'conn, Conn, S, Columns, Filters, MutationFiltered>
where
    Conn: QueryBuilder + 'conn,
    S: UpdateableTable + ProjectionShape + 'conn,
    Columns: UpdateAssignments + 'conn,
    Filters: PredicateNodes + 'conn,
{
    pub fn update(self) -> impl Future<Output = Result<u64, ErrorOf<Conn>>> + Send + 'conn
    where
        Conn: Connection + 'conn,
        Columns::Params: NoRuntimeParams,
        Filters::Params: NoRuntimeParams,
        // See `insert`: the future captures the query object, so require it `Send`.
        <Conn as QueryBuilder>::Update<'conn, S, (), Columns, Filters, ()>:
            ExecutableUpdateQuery<'conn, Columns, Filters, ()> + Send,
    {
        let query =
            <<Conn as QueryBuilder>::Update<'conn, S, (), Columns, Filters, ()> as UpdateQuery<
                'conn,
                Columns,
                Filters,
                (),
            >>::build(self.connection, self.alias, self.columns, self.filters, ());
        async move { ExecutableUpdateQuery::execute(&query).await }
    }

    pub fn update_returning<P>(
        self,
        projection: impl FnOnce(<S as ProjectionShape>::Exprs<'static>) -> P,
    ) -> Conn::Update<'conn, S, <P as ReturningProjection<'static>>::Shape, Columns, Filters, P>
    where
        // Aggregates are never valid in `RETURNING`, so require an aggregate-free projection.
        P: ReturningProjection<'static>
            + Projectable
            + crate::ProjectionClass<Class = crate::ScalarProjection>
            // Window functions are invalid in a RETURNING clause; `ReturnableProjection` excludes them
            + crate::ReturnableProjection
            + crate::ProjectionParams<Params = HNil>,
        <P::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
        Conn::Backend: SupportsReturning,
    {
        let table = <S as ProjectionShape>::exprs(self.alias);
        let projection = projection(table);
        <<Conn as QueryBuilder>::Update<
            'conn,
            S,
            <P as ReturningProjection<'static>>::Shape,
            Columns,
            Filters,
            P,
        > as UpdateQuery<'conn, Columns, Filters, P>>::build(
            self.connection,
            self.alias,
            self.columns,
            self.filters,
            projection,
        )
    }
}

/// A backend-specific update query object built from typed update state.
pub trait UpdateQuery<'builder, Columns, Filters, Returning>
where
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Builder: QueryBuilder + 'builder;
    type Table: UpdateableTable;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn build(
        builder: &'builder Self::Builder,
        alias: SourceAlias,
        columns: Columns,
        filters: Filters,
        returning: Returning,
    ) -> Self
    where
        Self: Sized;
}

/// An update query object that can execute or fetch rows through a connection.
pub trait ExecutableUpdateQuery<'conn, Columns, Filters, Returning>:
    UpdateQuery<'conn, Columns, Filters, Returning>
where
    Self::Builder: Connection,
    Columns: UpdateAssignments,
    Columns::Params: NoRuntimeParams,
    Filters: PredicateNodes,
    Filters::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>: Stream<Item = Result<Self::Row, ErrorOf<Self::Builder>>>
        + Send
        + RowsAffected
        + 'query
    where
        Self: 'query;

    fn execute(&self) -> impl Future<Output = Result<u64, ErrorOf<Self::Builder>>> + Send + '_;

    fn fetch<'query>(&'query self) -> Self::RowStream<'query>;

    fn collect<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Columns: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = RowsWithAffected<Self::Row, Self::Builder>> + Send + 'query
    where
        'conn: 'query,
        Columns: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        collect_rows_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Self::Row, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Columns: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let row = fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(self.fetch());
        async move {
            let (row, affected) = row.await?;
            let row = row
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)?;
            Ok((row, affected))
        }
    }

    fn fetch_optional_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = OptionalRowWithAffected<Self::Row, Self::Builder>> + Send + 'query
    where
        'conn: 'query,
        Columns: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Columns: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let row = fetch_optional_row::<Self::Builder, Self::Row, _>(self.fetch());
        async move {
            row.await?
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)
        }
    }

    fn fetch_optional<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Option<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Columns: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// An update query object that can be compiled into a backend-owned prepared statement.
pub trait PreparableUpdateQuery<'conn, Columns, Filters, Returning>:
    UpdateQuery<'conn, Columns, Filters, Returning>
where
    Self::Builder: Connection,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Params: HList;

    type Prepared<'prepared>: PreparedMutationQuery<
            'prepared,
            Builder = Self::Builder,
            Params = Self::Params,
            Row = Self::Row,
        > + 'prepared
    where
        Self: 'prepared,
        'conn: 'prepared,
        Columns: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<Output = Result<Self::Prepared<'prepared>, ErrorOf<Self::Builder>>> + 'prepared
    where
        'conn: 'prepared,
        Columns: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared;
}

/// A backend-specific delete query object built from typed delete state.
pub trait DeleteQuery<'builder, Filters, Returning>
where
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Builder: QueryBuilder + 'builder;
    type Table: TableProjection;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn build(
        builder: &'builder Self::Builder,
        alias: SourceAlias,
        filters: Filters,
        returning: Returning,
    ) -> Self
    where
        Self: Sized;
}

/// A delete query object that can be compiled into a backend-owned prepared statement.
pub trait PreparableDeleteQuery<'conn, Filters, Returning>:
    DeleteQuery<'conn, Filters, Returning>
where
    Self::Builder: Connection,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Params: HList;

    type Prepared<'prepared>: PreparedMutationQuery<
            'prepared,
            Builder = Self::Builder,
            Params = Self::Params,
            Row = Self::Row,
        > + 'prepared
    where
        Self: 'prepared,
        'conn: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<Output = Result<Self::Prepared<'prepared>, ErrorOf<Self::Builder>>> + 'prepared
    where
        'conn: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared;
}

/// A delete query object that can execute or fetch rows through a connection.
pub trait ExecutableDeleteQuery<'conn, Filters, Returning>:
    DeleteQuery<'conn, Filters, Returning>
where
    Self::Builder: Connection,
    Filters: PredicateNodes,
    Filters::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>: Stream<Item = Result<Self::Row, ErrorOf<Self::Builder>>>
        + Send
        + RowsAffected
        + 'query
    where
        Self: 'query;

    fn execute(&self) -> impl Future<Output = Result<u64, ErrorOf<Self::Builder>>> + Send + '_;

    fn fetch<'query>(&'query self) -> Self::RowStream<'query>;

    fn collect<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = RowsWithAffected<Self::Row, Self::Builder>> + Send + 'query
    where
        'conn: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        collect_rows_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Self::Row, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let row = fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(self.fetch());
        async move {
            let (row, affected) = row.await?;
            let row = row
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)?;
            Ok((row, affected))
        }
    }

    fn fetch_optional_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = OptionalRowWithAffected<Self::Row, Self::Builder>> + Send + 'query
    where
        'conn: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let row = fetch_optional_row::<Self::Builder, Self::Row, _>(self.fetch());
        async move {
            row.await?
                .ok_or_else(<<Self::Builder as QueryBuilder>::Backend as Backend>::no_rows_error)
        }
    }

    fn fetch_optional<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Option<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Filters: 'query,
        Returning: 'query,
    {
        let rows = self.fetch();
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

async fn collect_rows<Conn, Row, Rows>(rows: Rows) -> Result<Vec<Row>, ErrorOf<Conn>>
where
    Conn: QueryBuilder,
    Rows: Stream<Item = Result<Row, ErrorOf<Conn>>> + Send,
{
    let mut rows = pin!(rows);
    let mut output = Vec::new();
    while let Some(row) = poll_fn(|cx| rows.as_mut().poll_next(cx)).await {
        output.push(row?);
    }
    Ok(output)
}

async fn collect_rows_with_affected<Conn, Row, Rows>(
    rows: Rows,
) -> Result<(Vec<Row>, u64), ErrorOf<Conn>>
where
    Conn: QueryBuilder,
    Rows: Stream<Item = Result<Row, ErrorOf<Conn>>> + RowsAffected + Send,
{
    let mut rows = pin!(rows);
    let mut output = Vec::new();
    while let Some(row) = poll_fn(|cx| rows.as_mut().poll_next(cx)).await {
        output.push(row?);
    }
    let affected = rows.as_ref().get_ref().rows_affected().unwrap_or(0);
    Ok((output, affected))
}

async fn fetch_optional_row_with_affected<Conn, Row, Rows>(
    rows: Rows,
) -> Result<(Option<Row>, u64), ErrorOf<Conn>>
where
    Conn: QueryBuilder,
    Rows: Stream<Item = Result<Row, ErrorOf<Conn>>> + RowsAffected + Send,
{
    let mut rows = pin!(rows);
    let mut first = None;
    while let Some(row) = poll_fn(|cx| rows.as_mut().poll_next(cx)).await {
        if first.is_none() {
            first = Some(row?);
        } else {
            row?;
        }
    }
    let affected = rows.as_ref().get_ref().rows_affected().unwrap_or(0);
    Ok((first, affected))
}

async fn fetch_optional_row<Conn, Row, Rows>(rows: Rows) -> Result<Option<Row>, ErrorOf<Conn>>
where
    Conn: QueryBuilder,
    Rows: Stream<Item = Result<Row, ErrorOf<Conn>>> + Send,
{
    let mut rows = pin!(rows);
    poll_fn(|cx| rows.as_mut().poll_next(cx)).await.transpose()
}

/// A projection value that can identify the query shape returned by `returning`.
#[doc(hidden)]
pub trait ReturningProjection<'scope>: Projectable {
    type Shape: ProjectionShape;
}

impl<'scope, K, Ast> ReturningProjection<'scope> for Expr<'scope, K, Ast>
where
    K: ExprKind + ProjectionShape,
    Ast: crate::ExprAst,
{
    type Shape = K;
}

impl<'scope, K> ReturningProjection<'scope> for ColumnRef<'scope, K>
where
    K: ExprKind + ProjectionShape,
{
    type Shape = K;
}

impl<'scope, T> ReturningProjection<'scope> for T
where
    T: ExprKind<Value = T> + ProjectionShape + Clone,
{
    type Shape = T;
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct RootSource<S>
where
    S: TableProjection,
{
    alias: SourceAlias,
    _phantom: PhantomData<S>,
}

impl<S> RootSource<S>
where
    S: TableProjection,
{
    fn new(alias: SourceAlias) -> Self {
        Self {
            alias,
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct InnerJoinSource<'scope, S, P, PredicateAst>
where
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    alias: SourceAlias,
    on: Predicate<'scope, P, PredicateAst>,
    _phantom: PhantomData<S>,
}

impl<'scope, S, P, PredicateAst> InnerJoinSource<'scope, S, P, PredicateAst>
where
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    fn new(alias: SourceAlias, on: Predicate<'scope, P, PredicateAst>) -> Self {
        Self {
            alias,
            on,
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct LeftJoinSource<'scope, S, P, PredicateAst>
where
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    alias: SourceAlias,
    on: Predicate<'scope, P, PredicateAst>,
    _phantom: PhantomData<S>,
}

impl<'scope, S, P, PredicateAst> LeftJoinSource<'scope, S, P, PredicateAst>
where
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    fn new(alias: SourceAlias, on: Predicate<'scope, P, PredicateAst>) -> Self {
        Self {
            alias,
            on,
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct RightJoinSource<'scope, S, P, PredicateAst>
where
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    alias: SourceAlias,
    on: Predicate<'scope, P, PredicateAst>,
    _phantom: PhantomData<S>,
}

impl<'scope, S, P, PredicateAst> RightJoinSource<'scope, S, P, PredicateAst>
where
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    fn new(alias: SourceAlias, on: Predicate<'scope, P, PredicateAst>) -> Self {
        Self {
            alias,
            on,
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct FullJoinSource<'scope, S, P, PredicateAst>
where
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    alias: SourceAlias,
    on: Predicate<'scope, P, PredicateAst>,
    _phantom: PhantomData<S>,
}

impl<'scope, S, P, PredicateAst> FullJoinSource<'scope, S, P, PredicateAst>
where
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    fn new(alias: SourceAlias, on: Predicate<'scope, P, PredicateAst>) -> Self {
        Self {
            alias,
            on,
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
pub trait SelectSink {
    type Error;
    type Backend: Backend;

    fn push_projection<Shape, P>(&mut self, projection: P) -> Result<(), Self::Error>
    where
        Shape: ProjectionShape,
        P: RenderProjectable<Self::Backend>;

    fn push_table_source<S>(&mut self, alias: SourceAlias) -> Result<(), Self::Error>
    where
        S: TableProjection;

    fn push_inner_join<S, P, PredicateAst>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, PredicateAst>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        PredicateAst: crate::RenderPredicateAst<Self::Backend>;

    fn push_left_join<S, P, PredicateAst>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, PredicateAst>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        PredicateAst: crate::RenderPredicateAst<Self::Backend>;

    fn push_right_join<S, P, PredicateAst>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, PredicateAst>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        PredicateAst: crate::RenderPredicateAst<Self::Backend>;

    fn push_full_join<S, P, PredicateAst>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, PredicateAst>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        PredicateAst: crate::RenderPredicateAst<Self::Backend>;

    /// A `CROSS JOIN` (Cartesian product) — no `ON` condition.
    fn push_cross_join<S>(&mut self, alias: SourceAlias) -> Result<(), Self::Error>
    where
        S: TableProjection;

    fn push_filter<P, PredicateAst>(
        &mut self,
        predicate: Predicate<'_, P, PredicateAst>,
    ) -> Result<(), Self::Error>
    where
        P: PredicateKind,
        PredicateAst: crate::RenderPredicateAst<Self::Backend>;

    fn push_group<K, Ast>(&mut self, key: &Expr<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<Self::Backend>;

    fn push_having<P, PredicateAst>(
        &mut self,
        predicate: Predicate<'_, P, PredicateAst>,
    ) -> Result<(), Self::Error>
    where
        P: PredicateKind,
        PredicateAst: crate::RenderPredicateAst<Self::Backend>;

    fn push_order<K, Ast>(&mut self, order: Order<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<Self::Backend>;

    fn set_limit(&mut self, rows: usize) -> Result<(), Self::Error>;

    fn set_offset(&mut self, rows: usize) -> Result<(), Self::Error>;

    /// Mark the select as `DISTINCT`. Called before the projection is pushed (see
    /// [`Selected::lower_into`]), so the rendered `DISTINCT` keyword lands between `SELECT` and the
    /// column list. Defaulted to a no-op so sinks that don't render it need no change.
    fn set_distinct(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[doc(hidden)]
pub trait SourceSpec {
    type Params: HList;

    /// Push any CTE definitions this source contributes to the query's `WITH` clause. A table or view
    /// source contributes none (the default); a CTE source contributes its [`CteDef`]. The collected
    /// defs are de-duplicated and rendered as the `WITH` prefix before the main `SELECT`.
    fn collect_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        let _ = ctes;
    }
}

/// Pushes `S`'s [`CteDef`](crate::CteDef) onto the collected `WITH` set when `S` is a CTE. Shared by
/// every source kind's [`SourceSpec::collect_ctes`].
fn collect_source_cte<S>(ctes: &mut Vec<&'static dyn crate::CteDef>)
where
    S: QuerySource,
{
    if let Some(def) = S::cte_def() {
        ctes.push(def);
    }
}

/// Backend-parameterized source rendering (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderSourceSpec<B>: SourceSpec
where
    B: Backend,
{
    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;
}

impl<S> SourceSpec for RootSource<S>
where
    S: QuerySource,
{
    type Params = HNil;

    fn collect_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        collect_source_cte::<S>(ctes);
    }
}

impl<S, B> RenderSourceSpec<B> for RootSource<S>
where
    S: QuerySource,
    B: Backend,
{
    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        sink.push_table_source::<S>(self.alias)
    }
}

impl<S, P, PredicateAst> SourceSpec for InnerJoinSource<'_, S, P, PredicateAst>
where
    S: QuerySource,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    type Params = PredicateAst::Params;

    fn collect_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        collect_source_cte::<S>(ctes);
    }
}

impl<S, P, PredicateAst, B> RenderSourceSpec<B> for InnerJoinSource<'_, S, P, PredicateAst>
where
    S: QuerySource,
    P: PredicateKind,
    PredicateAst: crate::RenderPredicateAst<B>,
    B: Backend,
{
    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        sink.push_inner_join::<S, P, PredicateAst>(self.alias, self.on.clone())
    }
}

impl<S, P, PredicateAst> SourceSpec for LeftJoinSource<'_, S, P, PredicateAst>
where
    S: QuerySource,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    type Params = PredicateAst::Params;

    fn collect_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        collect_source_cte::<S>(ctes);
    }
}

impl<S, P, PredicateAst, B> RenderSourceSpec<B> for LeftJoinSource<'_, S, P, PredicateAst>
where
    S: QuerySource,
    P: PredicateKind,
    PredicateAst: crate::RenderPredicateAst<B>,
    B: Backend,
{
    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        sink.push_left_join::<S, P, PredicateAst>(self.alias, self.on.clone())
    }
}

impl<S, P, PredicateAst> SourceSpec for RightJoinSource<'_, S, P, PredicateAst>
where
    S: QuerySource,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    type Params = PredicateAst::Params;

    fn collect_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        collect_source_cte::<S>(ctes);
    }
}

impl<S, P, PredicateAst, B> RenderSourceSpec<B> for RightJoinSource<'_, S, P, PredicateAst>
where
    S: QuerySource,
    P: PredicateKind,
    PredicateAst: crate::RenderPredicateAst<B>,
    B: Backend,
{
    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        sink.push_right_join::<S, P, PredicateAst>(self.alias, self.on.clone())
    }
}

impl<S, P, PredicateAst> SourceSpec for FullJoinSource<'_, S, P, PredicateAst>
where
    S: QuerySource,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    type Params = PredicateAst::Params;

    fn collect_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        collect_source_cte::<S>(ctes);
    }
}

impl<S, P, PredicateAst, B> RenderSourceSpec<B> for FullJoinSource<'_, S, P, PredicateAst>
where
    S: QuerySource,
    P: PredicateKind,
    PredicateAst: crate::RenderPredicateAst<B>,
    B: Backend,
{
    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        sink.push_full_join::<S, P, PredicateAst>(self.alias, self.on.clone())
    }
}

/// `CROSS JOIN` source — a Cartesian product, with no `ON` condition (so no predicate params).
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct CrossJoinSource<S> {
    alias: SourceAlias,
    _phantom: PhantomData<S>,
}

impl<S> CrossJoinSource<S> {
    fn new(alias: SourceAlias) -> Self {
        Self {
            alias,
            _phantom: PhantomData,
        }
    }
}

impl<S> SourceSpec for CrossJoinSource<S>
where
    S: QuerySource,
{
    type Params = crate::HNil;

    fn collect_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        collect_source_cte::<S>(ctes);
    }
}

impl<S, B> RenderSourceSpec<B> for CrossJoinSource<S>
where
    S: QuerySource,
    B: Backend,
{
    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        sink.push_cross_join::<S>(self.alias)
    }
}

/// Marker for a select with no source tables.
#[doc(hidden)]
pub struct NoSources<'conn, Conn>
where
    Conn: QueryBuilder,
{
    connection: &'conn Conn,
    depth: usize,
}

impl<'conn, Conn> NoSources<'conn, Conn>
where
    Conn: QueryBuilder,
{
    fn new(connection: &'conn Conn, depth: usize) -> Self {
        Self { connection, depth }
    }
}

/// Marker for a select with no filters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoFilters;

/// Marker for a select with no ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoOrdering;

/// A typed select AST state.
pub(crate) struct Select<Sources, Filters, Ordering, Projection> {
    sources: Sources,
    filters: Filters,
    ordering: Ordering,
    projection: Projection,
}

impl<Sources, Filters, Ordering, Projection> Select<Sources, Filters, Ordering, Projection> {
    fn new(sources: Sources, filters: Filters, ordering: Ordering, projection: Projection) -> Self {
        Self {
            sources,
            filters,
            ordering,
            projection,
        }
    }
}

impl<'conn, Conn, Projection> Select<NoSources<'conn, Conn>, NoFilters, NoOrdering, Projection>
where
    Conn: QueryBuilder + 'conn,
    Projection: Projectable,
{
    fn into_selected<'scope, Shape>(
        self,
    ) -> Selected<'scope, NoSources<'conn, Conn>, Shape, Projection>
    where
        Shape: ProjectionShape,
    {
        _ = self.filters;
        _ = self.ordering;
        Selected::new(self.sources, self.projection)
    }
}

#[doc(hidden)]
pub struct Selected<'scope, Base, Shape, Projection>
where
    Shape: ProjectionShape,
    Projection: Projectable,
{
    base: Base,
    projection: Projection,
    _shape: PhantomData<(&'scope (), Shape)>,
}

impl<'scope, Base, Shape, Projection> Selected<'scope, Base, Shape, Projection>
where
    Shape: ProjectionShape,
    Projection: Projectable,
{
    fn new(base: Base, projection: Projection) -> Self {
        Self {
            base,
            projection,
            _shape: PhantomData,
        }
    }
}

impl<'scope, Base, Shape, Projection> Clone for Selected<'scope, Base, Shape, Projection>
where
    Base: Clone,
    Shape: ProjectionShape,
    Projection: Projectable,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            projection: self.projection.clone(),
            _shape: PhantomData,
        }
    }
}

impl<'scope, Base, Shape, Projection> Selected<'scope, Base, Shape, Projection>
where
    Shape: ProjectionShape,
    Projection: Projectable,
{
    fn connection<'conn, Conn>(&self) -> &'conn Conn
    where
        Conn: QueryBuilder + 'conn,
        Base: SelectAst<'conn, 'scope, Conn>,
    {
        self.base.connection()
    }

    /// The de-duplicated CTE definitions this select references through its `FROM`/`JOIN` sources, in
    /// first-seen order. The renderer emits these as the `WITH` prefix before the main `SELECT`.
    #[doc(hidden)]
    pub fn collect_ctes<'conn, Conn>(&self) -> Vec<&'static dyn crate::CteDef>
    where
        Conn: QueryBuilder + 'conn,
        Base: SelectAst<'conn, 'scope, Conn>,
    {
        let mut ctes = Vec::new();
        self.base.collect_ctes_into(&mut ctes);
        // A CTE referenced from several sources is collected once: keep the first occurrence.
        let mut seen = std::collections::HashSet::new();
        ctes.retain(|def| seen.insert(def.name()));
        ctes
    }

    #[doc(hidden)]
    pub fn lower_into<'conn, Conn, Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Conn: QueryBuilder + 'conn,
        Base: RenderSelectAst<'conn, 'scope, Conn, Sink::Backend>,
        Sink: SelectSink,
        Projection: RenderProjectable<Sink::Backend>,
    {
        // Before the projection (DISTINCT renders between SELECT and the column list).
        self.base.lower_distinct_into(sink)?;
        sink.push_projection::<Shape, _>(self.projection.clone())?;
        self.base.lower_sources_into(sink)?;
        self.base.lower_filters_into(sink)?;
        self.base.lower_groups_into(sink)?;
        self.base.lower_havings_into(sink)?;
        self.base.lower_orders_into(sink)?;
        self.base.lower_bounds_into(sink)
    }
}

/// A finished subquery — a [`Selected`] chain plus the connection type it was built against — erased
/// behind the [`Subquery`]/[`RenderSubquery`] traits so it can be embedded in an
/// expression/predicate AST and rendered without the embedding visitor naming the connection type.
#[doc(hidden)]
pub struct SubquerySelect<'conn, 'scope, Conn, Base, Shape, Projection>
where
    Conn: QueryBuilder,
    Shape: ProjectionShape,
    Projection: Projectable,
{
    selected: Selected<'scope, Base, Shape, Projection>,
    _conn: PhantomData<fn() -> &'conn Conn>,
}

impl<'conn, 'scope, Conn, Base, Shape, Projection> Clone
    for SubquerySelect<'conn, 'scope, Conn, Base, Shape, Projection>
where
    Conn: QueryBuilder,
    Base: Clone,
    Shape: ProjectionShape,
    Projection: Projectable,
{
    fn clone(&self) -> Self {
        Self {
            selected: self.selected.clone(),
            _conn: PhantomData,
        }
    }
}

/// Backend-independent facts about an embedded subquery: its runtime-parameter shape, so the outer
/// query's [`Params`](SelectAst::Params) can absorb them in render order. Any subquery (including a
/// multi-column or `SELECT 1` one used with `EXISTS`) satisfies this.
#[doc(hidden)]
pub trait Subquery: Clone {
    type Params: HList;
}

/// A subquery that projects exactly one column, usable where a single value is expected
/// (`IN (subquery)`, a scalar subquery). [`OutputKind`](Self::OutputKind) is that column's
/// expression kind — the *kind*, not just its value type — so a `ColumnType` newtype and a column's
/// nullability survive into the surrounding expression. Multi-column/table projections do not
/// implement [`crate::ExprKind`] for their shape, so they are rejected from these positions.
#[doc(hidden)]
pub trait ScalarSubquery: Subquery {
    type OutputKind: crate::ExprKind;
}

/// Backend-parameterized rendering of an embedded subquery (mirror of [`RenderSelectAst`]). The
/// connection type is captured by the implementor, not the method, so an
/// [`ExprVisitor`](crate::ExprVisitor)/[`PredicateAstVisitor`](crate::PredicateAstVisitor) can render
/// a nested SELECT knowing only the backend.
#[doc(hidden)]
pub trait RenderSubquery<B>: Subquery
where
    B: Backend,
{
    fn lower_subquery<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;
}

impl<'conn, 'scope, Conn, Base, Shape, Projection> Subquery
    for SubquerySelect<'conn, 'scope, Conn, Base, Shape, Projection>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn> + Clone,
    Shape: ProjectionShape,
    Projection: Projectable + crate::ProjectionParams,
    // Render order is projection → sources → filters → … , so the projection's params come first.
    <Projection as crate::ProjectionParams>::Params:
        crate::HAppend<<Base as SelectAst<'conn, 'scope, Conn>>::Params>,
{
    type Params = <<Projection as crate::ProjectionParams>::Params as crate::HAppend<
        <Base as SelectAst<'conn, 'scope, Conn>>::Params,
    >>::Output;
}

// A single-column projection's `Shape` is the projected column's kind, which is itself an
// `ExprKind` (a table/tuple `Shape` is not), so this impl both surfaces the kind and enforces the
// single-column requirement for `IN`/scalar subqueries.
impl<'conn, 'scope, Conn, Base, Shape, Projection> ScalarSubquery
    for SubquerySelect<'conn, 'scope, Conn, Base, Shape, Projection>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn> + Clone,
    Shape: ProjectionShape + crate::ExprKind,
    Projection: Projectable + crate::ProjectionParams,
    <Projection as crate::ProjectionParams>::Params:
        crate::HAppend<<Base as SelectAst<'conn, 'scope, Conn>>::Params>,
{
    type OutputKind = Shape;
}

impl<'conn, 'scope, Conn, Base, Shape, Projection, B> RenderSubquery<B>
    for SubquerySelect<'conn, 'scope, Conn, Base, Shape, Projection>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B> + Clone,
    Shape: ProjectionShape,
    Projection: crate::RenderProjectable<B> + crate::ProjectionParams,
    <Projection as crate::ProjectionParams>::Params:
        crate::HAppend<<Base as SelectAst<'conn, 'scope, Conn>>::Params>,
    B: Backend,
{
    fn lower_subquery<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.selected.lower_into::<Conn, Sink>(sink)
    }
}

/// A handle, passed to a correlated-predicate closure, for building subqueries that share the outer
/// query's connection and `'scope` (so outer columns may be referenced — i.e. correlation) while
/// nesting deeper, so the subquery's source aliases (`q{depth}_…`) never collide with the outer
/// query's.
///
/// Each [`from`](Self::from) hands out a fresh, larger depth than the last, so building several
/// subqueries from one handle — including reusing a captured handle inside an already-started
/// subquery — never reuses an alias (which would silently corrupt a correlation predicate). The
/// first `from` uses the handle's base depth, matching the simple single-subquery case.
#[doc(hidden)]
pub struct Subqueries<'conn, 'scope, Conn>
where
    Conn: QueryBuilder,
{
    connection: &'conn Conn,
    next_depth: std::cell::Cell<usize>,
    _scope: PhantomData<&'scope ()>,
}

impl<'conn, 'scope, Conn> Subqueries<'conn, 'scope, Conn>
where
    Conn: QueryBuilder + 'conn,
{
    fn new(connection: &'conn Conn, depth: usize) -> Self {
        Self {
            connection,
            next_depth: std::cell::Cell::new(depth),
            _scope: PhantomData,
        }
    }

    /// Start a subquery from table `S`, sharing the outer query's scope. Allocates the next fresh
    /// depth so sibling and nested subqueries built from this handle get distinct source aliases.
    pub fn from<S>(
        &self,
    ) -> From<'conn, 'scope, Conn, HCons<<S as ProjectionShape>::Exprs<'scope>, HNil>, RootSource<S>>
    where
        S: QuerySource,
    {
        let depth = self.next_depth.get();
        self.next_depth.set(depth + 1);
        From::new(self.connection, depth)
    }
}

/// Combines a select chain's per-clause runtime params into render order
/// (`sources ++ filters ++ groups ++ havings ++ orders`), so a chain's [`Params`](SelectAst::Params)
/// shape matches how placeholders are numbered at render time regardless of the order the builder
/// methods were called in. Runtime (`param`) placeholders are numbered as they render, while the
/// `Params` shape is what callers bind against; keeping the two aligned is what makes out-of-order
/// clause building (e.g. `order_by(..).where_(..)`) bind correctly.
#[doc(hidden)]
pub trait RenderOrderedParams {
    type Params: HList;
}

impl<Sources, Filters, Groups, Havings, Orders> RenderOrderedParams
    for (Sources, Filters, Groups, Havings, Orders)
where
    Sources: HList + crate::HAppend<Filters>,
    Filters: HList,
    Groups: HList,
    Havings: HList,
    Orders: HList,
    <Sources as crate::HAppend<Filters>>::Output: crate::HAppend<Groups>,
    <<Sources as crate::HAppend<Filters>>::Output as crate::HAppend<Groups>>::Output:
        crate::HAppend<Havings>,
    <<<Sources as crate::HAppend<Filters>>::Output as crate::HAppend<Groups>>::Output as crate::HAppend<
        Havings,
    >>::Output: crate::HAppend<Orders>,
{
    type Params = <<<<Sources as crate::HAppend<Filters>>::Output as crate::HAppend<Groups>>::Output
        as crate::HAppend<Havings>>::Output as crate::HAppend<Orders>>::Output;
}

#[doc(hidden)]
pub trait SelectAst<'conn, 'scope, Conn>
where
    Conn: QueryBuilder,
{
    type Exprs: HList + Clone + ToTuple;

    /// The chain's runtime-parameter shape, in render order. Assembled from the per-clause buckets
    /// below via [`RenderOrderedParams`] so it stays aligned with placeholder numbering even when
    /// clauses are added out of SQL order.
    type Params: HList;

    /// Runtime params contributed by `FROM`/`JOIN` sources (the first clauses to render).
    type SourceParams: HList;
    /// Runtime params contributed by `WHERE`.
    type FilterParams: HList;
    /// Runtime params contributed by `GROUP BY`.
    type GroupParams: HList;
    /// Runtime params contributed by `HAVING`.
    type HavingParams: HList;
    /// Runtime params contributed by `ORDER BY`.
    type OrderParams: HList;

    /// Which kinds of `ORDER BY` terms this chain has (see [`OrderNone`](crate::OrderNone) /
    /// [`OrderScalar`](crate::OrderScalar) / [`OrderAggregate`](crate::OrderAggregate) /
    /// [`OrderMixed`](crate::OrderMixed)). `select` requires the ordering match the projection class.
    type OrderClass;

    /// Whether this chain has a `GROUP BY` ([`Grouped`](crate::Grouped) /
    /// [`Ungrouped`](crate::Ungrouped)). A grouped chain relaxes the homogeneous-projection and
    /// order-compatibility rules `select` otherwise enforces (see [`ValidSelect`](crate::ValidSelect)).
    type Grouped;

    fn depth(&self) -> usize;

    fn connection(&self) -> &'conn Conn;

    fn exprs(&self) -> Self::Exprs;

    /// Walk this chain's `FROM`/`JOIN` sources, pushing each CTE source's [`CteDef`] so the renderer
    /// can emit the `WITH` prefix. Backend-independent (a `CteDef` is collected from the source type
    /// alone); wrapper nodes forward to their base, source nodes add their own source's contribution.
    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>);
}

/// Backend-parameterized select lowering (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderSelectAst<'conn, 'scope, Conn, B>: SelectAst<'conn, 'scope, Conn>
where
    Conn: QueryBuilder,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;

    /// Communicate `DISTINCT`-ness to the sink before the projection is pushed. Defaulted to a no-op;
    /// wrapper nodes forward to their base and [`Distinct`] flips the sink flag.
    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        let _ = sink;
        Ok(())
    }

    fn lower_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        // SQL clause order: WHERE → GROUP BY → HAVING → ORDER BY → LIMIT/OFFSET.
        self.lower_sources_into(sink)?;
        self.lower_filters_into(sink)?;
        self.lower_groups_into(sink)?;
        self.lower_havings_into(sink)?;
        self.lower_orders_into(sink)?;
        self.lower_bounds_into(sink)
    }
}

impl<'conn, 'scope, Conn> SelectAst<'conn, 'scope, Conn> for NoSources<'conn, Conn>
where
    Conn: QueryBuilder + 'conn,
{
    type Exprs = HNil;
    type Params = HNil;
    type SourceParams = HNil;
    type FilterParams = HNil;
    type GroupParams = HNil;
    type HavingParams = HNil;
    type OrderParams = HNil;
    type OrderClass = crate::OrderNone;
    type Grouped = crate::Ungrouped;

    fn depth(&self) -> usize {
        self.depth
    }

    fn connection(&self) -> &'conn Conn {
        self.connection
    }

    fn exprs(&self) -> Self::Exprs {
        HNil
    }

    fn collect_ctes_into(&self, _ctes: &mut Vec<&'static dyn crate::CteDef>) {}
}

impl<'conn, 'scope, Conn, B> RenderSelectAst<'conn, 'scope, Conn, B> for NoSources<'conn, Conn>
where
    Conn: QueryBuilder + 'conn,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_filters_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_groups_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_havings_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_orders_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_bounds_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }
}

/// A consuming, source-first select builder carrying typed sources.
pub struct From<'conn, 'scope, Conn, Exprs, Source>
where
    Conn: QueryBuilder,
    Exprs: HList,
    Source: SourceSpec,
{
    connection: &'conn Conn,
    depth: usize,
    exprs: Exprs,
    source: Source,
    _scope: PhantomData<&'scope ()>,
}

impl<'conn, 'scope, Conn, S>
    From<'conn, 'scope, Conn, HCons<<S as ProjectionShape>::Exprs<'scope>, HNil>, RootSource<S>>
where
    Conn: QueryBuilder + 'conn,
    S: QuerySource,
{
    pub(crate) fn new(connection: &'conn Conn, depth: usize) -> Self {
        let alias = SourceAlias::new(depth, 0);
        Self {
            connection,
            depth,
            exprs: HCons {
                head: S::exprs(alias),
                tail: HNil,
            },
            source: RootSource::new(alias),
            _scope: PhantomData,
        }
    }

    pub fn where_<P, PredicateAst>(
        self,
        predicate: impl FnOnce(
            <S as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope, P, PredicateAst>,
    ) -> Where<'scope, Self, P, PredicateAst>
    where
        P: PredicateKind,
        PredicateAst: crate::PredicateAst + crate::NonAggregatePredicate,
        <S as ProjectionShape>::Exprs<'scope>: Clone,
    {
        let predicate = predicate(self.exprs.head.clone());
        Where {
            base: self,
            predicate,
        }
    }

    /// Explicitly mark a delete as intentionally affecting every row.
    pub fn all(self) -> AllRows<Self> {
        AllRows { base: self }
    }
}

impl<'conn, 'scope, Conn, Exprs, Source> SelectAst<'conn, 'scope, Conn>
    for From<'conn, 'scope, Conn, Exprs, Source>
where
    Conn: QueryBuilder + 'conn,
    Exprs: HList + Clone + ToTuple,
    Source: SourceSpec,
{
    type Exprs = Exprs;
    type Params = Source::Params;
    type SourceParams = Source::Params;
    type FilterParams = HNil;
    type GroupParams = HNil;
    type HavingParams = HNil;
    type OrderParams = HNil;
    type OrderClass = crate::OrderNone;
    type Grouped = crate::Ungrouped;

    fn depth(&self) -> usize {
        self.depth
    }

    fn connection(&self) -> &'conn Conn {
        self.connection
    }

    fn exprs(&self) -> Self::Exprs {
        self.exprs.clone()
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.source.collect_ctes(ctes);
    }
}

impl<'conn, 'scope, Conn, Exprs, Source, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for From<'conn, 'scope, Conn, Exprs, Source>
where
    Conn: QueryBuilder + 'conn,
    Exprs: HList + Clone + ToTuple,
    Source: RenderSourceSpec<B>,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.source.push_source(sink)
    }

    fn lower_filters_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_groups_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_havings_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_orders_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }

    fn lower_bounds_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        Ok(())
    }
}

#[doc(hidden)]
pub struct Where<'scope, Base, P, PredicateAst>
where
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    base: Base,
    predicate: Predicate<'scope, P, PredicateAst>,
}

#[doc(hidden)]
pub struct AllRows<Base> {
    base: Base,
}

#[doc(hidden)]
pub struct OrderBy<'scope, Base, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    base: Base,
    order: Order<'scope, K, Ast>,
}

#[doc(hidden)]
pub struct GroupBy<'scope, Base, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    base: Base,
    key: Expr<'scope, K, Ast>,
}

#[doc(hidden)]
pub struct Having<'scope, Base, P, PredicateAst>
where
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    base: Base,
    predicate: Predicate<'scope, P, PredicateAst>,
}

/// Applies one or more `GROUP BY` keys onto a select chain, producing the nested [`GroupBy`]
/// node(s). Implemented for a single key (a column or expression) and for tuples of keys, so
/// [`SourceQuery::group_by`] accepts `group_by(|(u,)| u.id)` or `group_by(|(u,)| (u.id, u.name))`.
/// Each tuple arity delegates to the tail tuple, so the keys nest left-to-right and their params
/// thread through the existing single-key [`GroupBy`] node.
#[doc(hidden)]
pub trait GroupByKeys<'scope, Base> {
    type Output;

    fn apply(self, base: Base) -> Self::Output;
}

// No keys: a no-op, and the recursion base case for the tuple impls below.
impl<'scope, Base> GroupByKeys<'scope, Base> for () {
    type Output = Base;

    fn apply(self, base: Base) -> Self::Output {
        base
    }
}

impl<'scope, Base, K, Ast> GroupByKeys<'scope, Base> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    // A grouping item may not contain an aggregate (`GROUP BY COUNT(..)` is rejected by the database).
    Ast: ExprAst + crate::NonAggregateAst,
{
    type Output = GroupBy<'scope, Base, K, Ast>;

    fn apply(self, base: Base) -> Self::Output {
        GroupBy { base, key: self }
    }
}

impl<'scope, Base, K> GroupByKeys<'scope, Base> for ColumnRef<'scope, K>
where
    K: ExprKind,
{
    type Output = GroupBy<'scope, Base, K, ColumnExprAst<K>>;

    fn apply(self, base: Base) -> Self::Output {
        GroupBy {
            base,
            key: self.into_expr(),
        }
    }
}

macro_rules! impl_group_by_keys_tuple {
    () => {};
    ($head:ident $(, $tail:ident)*) => {
        impl<'scope, Base, $head, $($tail,)*> GroupByKeys<'scope, Base> for ($head, $($tail,)*)
        where
            $head: GroupByKeys<'scope, Base>,
            ($($tail,)*): GroupByKeys<'scope, <$head as GroupByKeys<'scope, Base>>::Output>,
        {
            type Output = <($($tail,)*) as GroupByKeys<
                'scope,
                <$head as GroupByKeys<'scope, Base>>::Output,
            >>::Output;

            #[allow(non_snake_case)]
            fn apply(self, base: Base) -> Self::Output {
                let ($head, $($tail,)*) = self;
                GroupByKeys::apply(($($tail,)*), GroupByKeys::apply($head, base))
            }
        }

        impl_group_by_keys_tuple!($($tail),*);
    };
}

impl_group_by_keys_tuple!(
    A0, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15
);

/// Applies one or more `HAVING` predicates onto a select chain, producing the nested [`Having`]
/// node(s). Implemented for a single predicate and for tuples of predicates, so
/// [`SourceQuery::having`] accepts `having(|(u,)| p)` or `having(|(u,)| (p1, p2))` (the predicates
/// are `AND`-joined). Mirrors [`GroupByKeys`]: each tuple arity delegates to the tail tuple, so the
/// predicates nest left-to-right and their params thread through the existing single-predicate
/// [`Having`] node.
#[doc(hidden)]
pub trait HavingPredicates<'scope, Base> {
    type Output;

    fn apply(self, base: Base) -> Self::Output;
}

// No predicates: a no-op, and the recursion base case for the tuple impls below.
impl<'scope, Base> HavingPredicates<'scope, Base> for () {
    type Output = Base;

    fn apply(self, base: Base) -> Self::Output {
        base
    }
}

impl<'scope, Base, P, Ast> HavingPredicates<'scope, Base> for Predicate<'scope, P, Ast>
where
    P: PredicateKind,
    Ast: crate::PredicateAst,
{
    type Output = Having<'scope, Base, P, Ast>;

    fn apply(self, base: Base) -> Self::Output {
        Having {
            base,
            predicate: self,
        }
    }
}

macro_rules! impl_having_predicates_tuple {
    () => {};
    ($head:ident $(, $tail:ident)*) => {
        impl<'scope, Base, $head, $($tail,)*> HavingPredicates<'scope, Base> for ($head, $($tail,)*)
        where
            $head: HavingPredicates<'scope, Base>,
            ($($tail,)*): HavingPredicates<'scope, <$head as HavingPredicates<'scope, Base>>::Output>,
        {
            type Output = <($($tail,)*) as HavingPredicates<
                'scope,
                <$head as HavingPredicates<'scope, Base>>::Output,
            >>::Output;

            #[allow(non_snake_case)]
            fn apply(self, base: Base) -> Self::Output {
                let ($head, $($tail,)*) = self;
                HavingPredicates::apply(($($tail,)*), HavingPredicates::apply($head, base))
            }
        }

        impl_having_predicates_tuple!($($tail),*);
    };
}

impl_having_predicates_tuple!(
    A0, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15
);

/// Applies one or more `ORDER BY` terms onto a select chain, producing the nested [`OrderBy`]
/// node(s). Implemented for a single ordering and for tuples of orderings, so
/// [`SourceQuery::order_by`] accepts `order_by(|(u,)| u.id.asc())` or
/// `order_by(|(u,)| (u.id.asc(), u.name.desc()))`. Mirrors [`GroupByKeys`]: each tuple arity
/// delegates to the tail tuple, so the terms nest left-to-right and their params and order-class
/// typestate thread through the existing single-term [`OrderBy`] node.
#[doc(hidden)]
pub trait OrderByTerms<'scope, Base> {
    type Output;

    fn apply(self, base: Base) -> Self::Output;
}

// No terms: a no-op, and the recursion base case for the tuple impls below.
impl<'scope, Base> OrderByTerms<'scope, Base> for () {
    type Output = Base;

    fn apply(self, base: Base) -> Self::Output {
        base
    }
}

impl<'scope, Base, K, Ast> OrderByTerms<'scope, Base> for Order<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    type Output = OrderBy<'scope, Base, K, Ast>;

    fn apply(self, base: Base) -> Self::Output {
        OrderBy { base, order: self }
    }
}

macro_rules! impl_order_by_terms_tuple {
    () => {};
    ($head:ident $(, $tail:ident)*) => {
        impl<'scope, Base, $head, $($tail,)*> OrderByTerms<'scope, Base> for ($head, $($tail,)*)
        where
            $head: OrderByTerms<'scope, Base>,
            ($($tail,)*): OrderByTerms<'scope, <$head as OrderByTerms<'scope, Base>>::Output>,
        {
            type Output = <($($tail,)*) as OrderByTerms<
                'scope,
                <$head as OrderByTerms<'scope, Base>>::Output,
            >>::Output;

            #[allow(non_snake_case)]
            fn apply(self, base: Base) -> Self::Output {
                let ($head, $($tail,)*) = self;
                OrderByTerms::apply(($($tail,)*), OrderByTerms::apply($head, base))
            }
        }

        impl_order_by_terms_tuple!($($tail),*);
    };
}

impl_order_by_terms_tuple!(
    A0, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15
);

#[doc(hidden)]
pub struct Limited<Base> {
    base: Base,
    rows: usize,
}

#[doc(hidden)]
pub struct Offset<Base> {
    base: Base,
    rows: usize,
}

/// Marks a select as `DISTINCT`. Wraps the chain and forwards everything to `Base`; the only effect
/// is flipping the sink's distinct flag during lowering (see `lower_distinct_into`).
#[doc(hidden)]
pub struct Distinct<Base> {
    base: Base,
}

/// Typestate marker used by generated mutation builders before a filter or all-rows intent exists.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationUnfiltered {}

/// Typestate marker used by generated mutation builders once a mutation is safe to execute.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationFiltered {}

#[doc(hidden)]
pub struct Join<Base, Expr, Source> {
    base: Base,
    expr: Expr,
    source: Source,
}

#[doc(hidden)]
pub struct LeftJoin<Base, Expr, Source> {
    base: Base,
    expr: Expr,
    source: Source,
}

#[doc(hidden)]
pub struct JoinTarget<Base, S> {
    base: Base,
    _source: PhantomData<S>,
}

#[doc(hidden)]
pub struct LeftJoinTarget<Base, S> {
    base: Base,
    _source: PhantomData<S>,
}

/// Outer join node shared by `right_join` and `full_join`: both nullable-wrap the **accumulated base**
/// columns (unlike `LeftJoin`, which only nullable-wraps the newly joined table). They differ only in
/// the stored `Expr` (the new table — non-null for right, `Maybe`-nullable for full) and the `Source`
/// (`RightJoinSource`/`FullJoinSource`, which render `RIGHT JOIN`/`FULL JOIN`).
#[doc(hidden)]
pub struct OuterJoin<Base, Expr, Source> {
    base: Base,
    expr: Expr,
    source: Source,
}

#[doc(hidden)]
pub struct RightJoinTarget<Base, S> {
    base: Base,
    _source: PhantomData<S>,
}

#[doc(hidden)]
pub struct FullJoinTarget<Base, S> {
    base: Base,
    _source: PhantomData<S>,
}

// Manual `Clone` impls for the select-chain typestate nodes. A subquery embeds a whole select chain
// inside a predicate/expression AST, and `PredicateAst`/`ExprAst` require `Clone` (predicates are
// cloned when lowered into a sink). The fields are all cheaply clonable — `&'conn Conn` is `Copy`,
// `exprs`/predicates/orders are `Clone` — so these impls deliberately do *not* require `Conn: Clone`
// the way `#[derive(Clone)]` would.

impl<'conn, Conn> Clone for NoSources<'conn, Conn>
where
    Conn: QueryBuilder,
{
    fn clone(&self) -> Self {
        Self {
            connection: self.connection,
            depth: self.depth,
        }
    }
}

impl<'conn, 'scope, Conn, Exprs, Source> Clone for From<'conn, 'scope, Conn, Exprs, Source>
where
    Conn: QueryBuilder,
    Exprs: HList + Clone,
    Source: SourceSpec + Clone,
{
    fn clone(&self) -> Self {
        Self {
            connection: self.connection,
            depth: self.depth,
            exprs: self.exprs.clone(),
            source: self.source.clone(),
            _scope: PhantomData,
        }
    }
}

impl<'scope, Base, P, PredicateAst> Clone for Where<'scope, Base, P, PredicateAst>
where
    Base: Clone,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            predicate: self.predicate.clone(),
        }
    }
}

impl<'scope, Base, K, Ast> Clone for OrderBy<'scope, Base, K, Ast>
where
    Base: Clone,
    K: ExprKind,
    Ast: ExprAst,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            order: self.order.clone(),
        }
    }
}

impl<'scope, Base, K, Ast> Clone for GroupBy<'scope, Base, K, Ast>
where
    Base: Clone,
    K: ExprKind,
    Ast: ExprAst,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            key: self.key.clone(),
        }
    }
}

impl<'scope, Base, P, PredicateAst> Clone for Having<'scope, Base, P, PredicateAst>
where
    Base: Clone,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            predicate: self.predicate.clone(),
        }
    }
}

impl<Base> Clone for Limited<Base>
where
    Base: Clone,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            rows: self.rows,
        }
    }
}

impl<Base> Clone for Offset<Base>
where
    Base: Clone,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            rows: self.rows,
        }
    }
}

impl<Base> Clone for Distinct<Base>
where
    Base: Clone,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
        }
    }
}

impl<Base, Expr, Source> Clone for Join<Base, Expr, Source>
where
    Base: Clone,
    Expr: Clone,
    Source: Clone,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            expr: self.expr.clone(),
            source: self.source.clone(),
        }
    }
}

impl<Base, Expr, Source> Clone for LeftJoin<Base, Expr, Source>
where
    Base: Clone,
    Expr: Clone,
    Source: Clone,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            expr: self.expr.clone(),
            source: self.source.clone(),
        }
    }
}

impl<Base, Expr, Source> Clone for OuterJoin<Base, Expr, Source>
where
    Base: Clone,
    Expr: Clone,
    Source: Clone,
{
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            expr: self.expr.clone(),
            source: self.source.clone(),
        }
    }
}

impl<'conn, 'scope, Conn, Base, P, PredicateAst> SelectAst<'conn, 'scope, Conn>
    for Where<'scope, Base, P, PredicateAst>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
    Base::FilterParams: crate::HAppend<PredicateAst::Params>,
    (
        Base::SourceParams,
        <Base::FilterParams as crate::HAppend<PredicateAst::Params>>::Output,
        Base::GroupParams,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
{
    type Exprs = Base::Exprs;
    type Params = <(
        Self::SourceParams,
        Self::FilterParams,
        Self::GroupParams,
        Self::HavingParams,
        Self::OrderParams,
    ) as RenderOrderedParams>::Params;
    type SourceParams = Base::SourceParams;
    type FilterParams = <Base::FilterParams as crate::HAppend<PredicateAst::Params>>::Output;
    type GroupParams = Base::GroupParams;
    type HavingParams = Base::HavingParams;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    type Grouped = Base::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, P, PredicateAst, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for Where<'scope, Base, P, PredicateAst>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    P: PredicateKind,
    PredicateAst: crate::RenderPredicateAst<B>,
    Base::FilterParams: crate::HAppend<PredicateAst::Params>,
    (
        Base::SourceParams,
        <Base::FilterParams as crate::HAppend<PredicateAst::Params>>::Output,
        Base::GroupParams,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)?;
        sink.push_filter(self.predicate.clone())
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

impl<'conn, 'scope, Conn, Base, K, Ast> SelectAst<'conn, 'scope, Conn>
    for OrderBy<'scope, Base, K, Ast>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    K: ExprKind,
    Ast: ExprAst + crate::AstProjectionClass,
    Base::OrderParams: crate::HAppend<Ast::Params>,
    Base::OrderClass: crate::ExtendOrderClass<<Ast as crate::AstProjectionClass>::Class>,
    (
        Base::SourceParams,
        Base::FilterParams,
        Base::GroupParams,
        Base::HavingParams,
        <Base::OrderParams as crate::HAppend<Ast::Params>>::Output,
    ): RenderOrderedParams,
{
    type Exprs = Base::Exprs;
    type Params = <(
        Self::SourceParams,
        Self::FilterParams,
        Self::GroupParams,
        Self::HavingParams,
        Self::OrderParams,
    ) as RenderOrderedParams>::Params;
    type SourceParams = Base::SourceParams;
    type FilterParams = Base::FilterParams;
    type GroupParams = Base::GroupParams;
    type HavingParams = Base::HavingParams;
    type OrderParams = <Base::OrderParams as crate::HAppend<Ast::Params>>::Output;
    type OrderClass = <Base::OrderClass as crate::ExtendOrderClass<
        <Ast as crate::AstProjectionClass>::Class,
    >>::Output;
    type Grouped = Base::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, K, Ast, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for OrderBy<'scope, Base, K, Ast>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    K: ExprKind,
    Ast: RenderAst<B> + crate::AstProjectionClass,
    Base::OrderParams: crate::HAppend<Ast::Params>,
    Base::OrderClass: crate::ExtendOrderClass<<Ast as crate::AstProjectionClass>::Class>,
    (
        Base::SourceParams,
        Base::FilterParams,
        Base::GroupParams,
        Base::HavingParams,
        <Base::OrderParams as crate::HAppend<Ast::Params>>::Output,
    ): RenderOrderedParams,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)?;
        sink.push_order(self.order.clone())
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

impl<'conn, 'scope, Conn, Base, K, Ast> SelectAst<'conn, 'scope, Conn>
    for GroupBy<'scope, Base, K, Ast>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    K: ExprKind,
    Ast: ExprAst,
    Base::GroupParams: crate::HAppend<Ast::Params>,
    (
        Base::SourceParams,
        Base::FilterParams,
        <Base::GroupParams as crate::HAppend<Ast::Params>>::Output,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
{
    type Exprs = Base::Exprs;
    type Params = <(
        Self::SourceParams,
        Self::FilterParams,
        Self::GroupParams,
        Self::HavingParams,
        Self::OrderParams,
    ) as RenderOrderedParams>::Params;
    type SourceParams = Base::SourceParams;
    type FilterParams = Base::FilterParams;
    type GroupParams = <Base::GroupParams as crate::HAppend<Ast::Params>>::Output;
    type HavingParams = Base::HavingParams;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    type Grouped = crate::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, K, Ast, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for GroupBy<'scope, Base, K, Ast>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    K: ExprKind,
    Ast: RenderAst<B>,
    Base::GroupParams: crate::HAppend<Ast::Params>,
    (
        Base::SourceParams,
        Base::FilterParams,
        <Base::GroupParams as crate::HAppend<Ast::Params>>::Output,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)?;
        sink.push_group(&self.key)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

impl<'conn, 'scope, Conn, Base, P, PredicateAst> SelectAst<'conn, 'scope, Conn>
    for Having<'scope, Base, P, PredicateAst>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
    Base::HavingParams: crate::HAppend<PredicateAst::Params>,
    PredicateAst: crate::PredicateColumns,
    Base::Grouped: crate::HavingTransition<<PredicateAst as crate::PredicateColumns>::Columns>,
    (
        Base::SourceParams,
        Base::FilterParams,
        Base::GroupParams,
        <Base::HavingParams as crate::HAppend<PredicateAst::Params>>::Output,
        Base::OrderParams,
    ): RenderOrderedParams,
{
    type Exprs = Base::Exprs;
    type Params = <(
        Self::SourceParams,
        Self::FilterParams,
        Self::GroupParams,
        Self::HavingParams,
        Self::OrderParams,
    ) as RenderOrderedParams>::Params;
    type SourceParams = Base::SourceParams;
    type FilterParams = Base::FilterParams;
    type GroupParams = Base::GroupParams;
    type HavingParams = <Base::HavingParams as crate::HAppend<PredicateAst::Params>>::Output;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    // A bare `HAVING` (no `GROUP BY`) makes this a whole-table aggregate; if the predicate references
    // a bare column the chain becomes `AggregateNeedsGroupBy` until a `group_by` rescues it. A
    // `GROUP BY` already present keeps the chain `Grouped`.
    type Grouped = <Base::Grouped as crate::HavingTransition<
        <PredicateAst as crate::PredicateColumns>::Columns,
    >>::Output;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, P, PredicateAst, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for Having<'scope, Base, P, PredicateAst>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    P: PredicateKind,
    PredicateAst: crate::RenderPredicateAst<B>,
    Base::HavingParams: crate::HAppend<PredicateAst::Params>,
    PredicateAst: crate::PredicateColumns,
    Base::Grouped: crate::HavingTransition<<PredicateAst as crate::PredicateColumns>::Columns>,
    (
        Base::SourceParams,
        Base::FilterParams,
        Base::GroupParams,
        <Base::HavingParams as crate::HAppend<PredicateAst::Params>>::Output,
        Base::OrderParams,
    ): RenderOrderedParams,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)?;
        sink.push_having(self.predicate.clone())
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for Limited<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Params = Base::Params;
    type SourceParams = Base::SourceParams;
    type FilterParams = Base::FilterParams;
    type GroupParams = Base::GroupParams;
    type HavingParams = Base::HavingParams;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    type Grouped = Base::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, B> RenderSelectAst<'conn, 'scope, Conn, B> for Limited<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)?;
        sink.set_limit(self.rows)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for Offset<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Params = Base::Params;
    type SourceParams = Base::SourceParams;
    type FilterParams = Base::FilterParams;
    type GroupParams = Base::GroupParams;
    type HavingParams = Base::HavingParams;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    type Grouped = Base::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, B> RenderSelectAst<'conn, 'scope, Conn, B> for Offset<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)?;
        sink.set_offset(self.rows)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for Distinct<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Params = Base::Params;
    type SourceParams = Base::SourceParams;
    type FilterParams = Base::FilterParams;
    type GroupParams = Base::GroupParams;
    type HavingParams = Base::HavingParams;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    type Grouped = Base::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, B> RenderSelectAst<'conn, 'scope, Conn, B> for Distinct<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        // Forward first (a nested DISTINCT is idempotent), then flip the flag for this node.
        self.base.lower_distinct_into(sink)?;
        sink.set_distinct()
    }
}

impl<'conn, 'scope, Conn, Base, Expr, Source> SelectAst<'conn, 'scope, Conn>
    for Join<Base, Expr, Source>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Base::Exprs: PushBack<Expr>,
    <Base::Exprs as PushBack<Expr>>::Output: Clone + ToTuple,
    Expr: Clone,
    Source: SourceSpec,
    Base::SourceParams: crate::HAppend<Source::Params>,
    (
        <Base::SourceParams as crate::HAppend<Source::Params>>::Output,
        Base::FilterParams,
        Base::GroupParams,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
{
    type Exprs = <Base::Exprs as PushBack<Expr>>::Output;
    type Params = <(
        Self::SourceParams,
        Self::FilterParams,
        Self::GroupParams,
        Self::HavingParams,
        Self::OrderParams,
    ) as RenderOrderedParams>::Params;
    type SourceParams = <Base::SourceParams as crate::HAppend<Source::Params>>::Output;
    type FilterParams = Base::FilterParams;
    type GroupParams = Base::GroupParams;
    type HavingParams = Base::HavingParams;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    type Grouped = Base::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs().push_back(self.expr.clone())
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
        self.source.collect_ctes(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, Expr, Source, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for Join<Base, Expr, Source>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    Base::Exprs: PushBack<Expr>,
    <Base::Exprs as PushBack<Expr>>::Output: Clone + ToTuple,
    Expr: Clone,
    Source: RenderSourceSpec<B>,
    Base::SourceParams: crate::HAppend<Source::Params>,
    (
        <Base::SourceParams as crate::HAppend<Source::Params>>::Output,
        Base::FilterParams,
        Base::GroupParams,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)?;
        self.source.push_source(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

impl<'conn, 'scope, Conn, Base, Expr, Source> SelectAst<'conn, 'scope, Conn>
    for LeftJoin<Base, Expr, Source>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Base::Exprs: PushBack<Expr>,
    <Base::Exprs as PushBack<Expr>>::Output: Clone + ToTuple,
    Expr: Clone,
    Source: SourceSpec,
    Base::SourceParams: crate::HAppend<Source::Params>,
    (
        <Base::SourceParams as crate::HAppend<Source::Params>>::Output,
        Base::FilterParams,
        Base::GroupParams,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
{
    type Exprs = <Base::Exprs as PushBack<Expr>>::Output;
    type Params = <(
        Self::SourceParams,
        Self::FilterParams,
        Self::GroupParams,
        Self::HavingParams,
        Self::OrderParams,
    ) as RenderOrderedParams>::Params;
    type SourceParams = <Base::SourceParams as crate::HAppend<Source::Params>>::Output;
    type FilterParams = Base::FilterParams;
    type GroupParams = Base::GroupParams;
    type HavingParams = Base::HavingParams;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    type Grouped = Base::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs().push_back(self.expr.clone())
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
        self.source.collect_ctes(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, Expr, Source, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for LeftJoin<Base, Expr, Source>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    Base::Exprs: PushBack<Expr>,
    <Base::Exprs as PushBack<Expr>>::Output: Clone + ToTuple,
    Expr: Clone,
    Source: RenderSourceSpec<B>,
    Base::SourceParams: crate::HAppend<Source::Params>,
    (
        <Base::SourceParams as crate::HAppend<Source::Params>>::Output,
        Base::FilterParams,
        Base::GroupParams,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)?;
        self.source.push_source(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

impl<'conn, 'scope, Conn, Base, Expr, Source> SelectAst<'conn, 'scope, Conn>
    for OuterJoin<Base, Expr, Source>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Base::Exprs: crate::IntoNullableExprs,
    <Base::Exprs as crate::IntoNullableExprs>::Output: PushBack<Expr>,
    <<Base::Exprs as crate::IntoNullableExprs>::Output as PushBack<Expr>>::Output: Clone + ToTuple,
    Expr: Clone,
    Source: SourceSpec,
    Base::SourceParams: crate::HAppend<Source::Params>,
    (
        <Base::SourceParams as crate::HAppend<Source::Params>>::Output,
        Base::FilterParams,
        Base::GroupParams,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
{
    // The accumulated base columns are nullable-wrapped (`RIGHT`/`FULL JOIN` can produce all-NULL base
    // rows); the new table's `Expr` is appended as supplied (non-null for right, `Maybe` for full).
    type Exprs = <<Base::Exprs as crate::IntoNullableExprs>::Output as PushBack<Expr>>::Output;
    type Params = <(
        Self::SourceParams,
        Self::FilterParams,
        Self::GroupParams,
        Self::HavingParams,
        Self::OrderParams,
    ) as RenderOrderedParams>::Params;
    type SourceParams = <Base::SourceParams as crate::HAppend<Source::Params>>::Output;
    type FilterParams = Base::FilterParams;
    type GroupParams = Base::GroupParams;
    type HavingParams = Base::HavingParams;
    type OrderParams = Base::OrderParams;
    type OrderClass = Base::OrderClass;
    type Grouped = Base::Grouped;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base
            .exprs()
            .into_nullable_exprs()
            .push_back(self.expr.clone())
    }

    fn collect_ctes_into(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.base.collect_ctes_into(ctes);
        self.source.collect_ctes(ctes);
    }
}

impl<'conn, 'scope, Conn, Base, Expr, Source, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for OuterJoin<Base, Expr, Source>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    Base::Exprs: crate::IntoNullableExprs,
    <Base::Exprs as crate::IntoNullableExprs>::Output: PushBack<Expr>,
    <<Base::Exprs as crate::IntoNullableExprs>::Output as PushBack<Expr>>::Output: Clone + ToTuple,
    Expr: Clone,
    Source: RenderSourceSpec<B>,
    Base::SourceParams: crate::HAppend<Source::Params>,
    (
        <Base::SourceParams as crate::HAppend<Source::Params>>::Output,
        Base::FilterParams,
        Base::GroupParams,
        Base::HavingParams,
        Base::OrderParams,
    ): RenderOrderedParams,
    B: Backend,
{
    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_sources_into(sink)?;
        self.source.push_source(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_groups_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_groups_into(sink)
    }

    fn lower_havings_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_havings_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_bounds_into(sink)
    }

    fn lower_distinct_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.base.lower_distinct_into(sink)
    }
}

pub trait SourceQuery<'conn, 'scope, Conn>: SelectAst<'conn, 'scope, Conn> + Sized
where
    Conn: QueryBuilder + 'conn,
{
    fn where_<P, PredicateAst>(
        self,
        predicate: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> Predicate<'scope, P, PredicateAst>,
    ) -> Where<'scope, Self, P, PredicateAst>
    where
        P: PredicateKind,
        PredicateAst: crate::PredicateAst + crate::NonAggregatePredicate,
    {
        let predicate = predicate(self.exprs().to_tuple());
        Where {
            base: self,
            predicate,
        }
    }

    /// Add one or more `ORDER BY` terms. The closure may return a single ordering or a tuple of
    /// them (`order_by(|(u,)| (u.id.asc(), u.name.desc()))` -> `ORDER BY id ASC, name DESC`);
    /// chaining `order_by` also accumulates terms.
    fn order_by<Orders>(
        self,
        orders: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> Orders,
    ) -> Orders::Output
    where
        Orders: OrderByTerms<'scope, Self>,
    {
        orders(self.exprs().to_tuple()).apply(self)
    }

    /// Add one or more `GROUP BY` keys. The closure may return a single key (a column or
    /// expression) or a tuple of keys (`group_by(|(u,)| (u.id, u.name))` -> `GROUP BY id, name`);
    /// chaining `group_by` also accumulates keys. A grouped query may select grouping keys
    /// alongside aggregates (the database validates that non-aggregate projected/ordered columns
    /// are grouping keys).
    fn group_by<Keys>(
        self,
        keys: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> Keys,
    ) -> Keys::Output
    where
        Keys: GroupByKeys<'scope, Self>,
    {
        keys(self.exprs().to_tuple()).apply(self)
    }

    /// Add one or more `HAVING` predicates. The closure may return a single predicate or a tuple of
    /// predicates (`having(|(u,)| (p1, p2))` -> `HAVING p1 AND p2`); chaining `having` also
    /// accumulates predicates. Unlike `where_`, `HAVING` may reference aggregates.
    fn having<Preds>(
        self,
        predicates: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> Preds,
    ) -> Preds::Output
    where
        Preds: HavingPredicates<'scope, Self>,
    {
        predicates(self.exprs().to_tuple()).apply(self)
    }

    /// Like [`where_`](Self::where_), but the closure also receives a [`Subqueries`] handle for
    /// building correlated subqueries (`IN (subquery)`, `EXISTS`, scalar) that may reference the
    /// outer query's columns. The handle nests subqueries one level deeper so their aliases never
    /// collide with the outer query's.
    fn where_correlated<P, PredicateAst>(
        self,
        predicate: impl FnOnce(
            <Self::Exprs as ToTuple>::Tuple,
            Subqueries<'conn, 'scope, Conn>,
        ) -> Predicate<'scope, P, PredicateAst>,
    ) -> Where<'scope, Self, P, PredicateAst>
    where
        P: PredicateKind,
        PredicateAst: crate::PredicateAst + crate::NonAggregatePredicate,
    {
        let subqueries = Subqueries::new(self.connection(), self.depth() + 1);
        let predicate = predicate(self.exprs().to_tuple(), subqueries);
        Where {
            base: self,
            predicate,
        }
    }

    /// Finish this chain as an embeddable subquery rather than an executable query. The projection
    /// must select exactly one column; the resulting [`SubquerySelect`] carries that column's type as
    /// its [`Subquery::Output`] so an `IN (subquery)` or scalar use can be type-checked.
    fn select_subquery<P>(
        self,
        projection: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> P,
    ) -> SubquerySelect<'conn, 'scope, Conn, Self, <P as ReturningProjection<'scope>>::Shape, P>
    where
        P: ReturningProjection<'scope> + Projectable,
        <Self as SelectAst<'conn, 'scope, Conn>>::Grouped:
            crate::ValidSelect<P, <Self as SelectAst<'conn, 'scope, Conn>>::OrderClass>,
        <P as ReturningProjection<'scope>>::Shape: ProjectionShape,
    {
        let exprs = self.exprs();
        let projection = projection(exprs.to_tuple());
        let selected = Selected::<'scope, _, <P as ReturningProjection<'scope>>::Shape, _>::new(
            self, projection,
        );
        SubquerySelect {
            selected,
            _conn: PhantomData,
        }
    }

    fn limit(self, rows: usize) -> Limited<Self> {
        Limited { base: self, rows }
    }

    fn offset(self, rows: usize) -> Offset<Self> {
        Offset { base: self, rows }
    }

    /// Render the select as `SELECT DISTINCT …` (deduplicate whole rows). Composes with the other
    /// clauses regardless of where in the chain it is called.
    ///
    /// Note: SQL requires every `ORDER BY` expression to also appear in the projection when
    /// `DISTINCT` is used. Ordering a distinct query by a column that is not selected
    /// (`distinct().order_by(|(u,)| u.id.asc()).select(|(u,)| u.name)`) is rejected by the database at
    /// execution time. This is not yet enforced at compile time (tracked as a follow-up).
    fn distinct(self) -> Distinct<Self> {
        Distinct { base: self }
    }

    fn join<S>(self) -> JoinTarget<Self, S>
    where
        S: QuerySource,
    {
        JoinTarget {
            base: self,
            _source: PhantomData,
        }
    }

    fn left_join<S>(self) -> LeftJoinTarget<Self, S>
    where
        S: QuerySource,
    {
        LeftJoinTarget {
            base: self,
            _source: PhantomData,
        }
    }

    /// `RIGHT JOIN`: the joined table's columns stay non-null while the accumulated base columns
    /// become nullable (an unmatched right-side row yields all-NULL base columns). Both backends
    /// support it.
    fn right_join<S>(self) -> RightJoinTarget<Self, S>
    where
        S: QuerySource,
    {
        RightJoinTarget {
            base: self,
            _source: PhantomData,
        }
    }

    /// `FULL [OUTER] JOIN`: both the joined table and the accumulated base become nullable.
    /// Gated to backends whose dialect supports it ([`SupportsFullJoin`] — PostgreSQL, and the view
    /// model backend); MySQL has no `FULL JOIN`, so this does not compile against it.
    fn full_join<S>(self) -> FullJoinTarget<Self, S>
    where
        S: QuerySource,
        Conn::Backend: crate::SupportsFullJoin,
    {
        FullJoinTarget {
            base: self,
            _source: PhantomData,
        }
    }

    /// `CROSS JOIN`: the Cartesian product of the current sources with `S`. There is no `ON` condition,
    /// and the joined table's columns stay non-null (a Cartesian product introduces no NULLs). Both
    /// backends support `CROSS JOIN`, so this needs no capability gate and — unlike the predicated joins
    /// — returns the joined query directly (no `.on()` step).
    fn cross_join<S>(self) -> Join<Self, <S as ProjectionShape>::Exprs<'scope>, CrossJoinSource<S>>
    where
        S: QuerySource,
    {
        let alias = SourceAlias::new(self.depth(), Self::Exprs::LEN);
        let right = S::exprs(alias);
        Join {
            base: self,
            expr: right,
            source: CrossJoinSource::new(alias),
        }
    }

    fn select<P>(
        self,
        projection: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> P,
    ) -> Conn::Select<'conn, 'scope, Self, <P as ReturningProjection<'scope>>::Shape, P>
    where
        // The projection must carry no runtime params: a projected scalar subquery renders before
        // the outer FROM, but the executable/prepared param shape is the source chain's, so a
        // `param` in the SELECT list would be an unbindable placeholder. Bare columns, aggregates,
        // arithmetic over literals, and correlated (column-referencing) scalar subqueries all have
        // empty projection params; only a runtime `param` in the SELECT list is rejected.
        P: ReturningProjection<'scope> + Projectable + crate::ProjectionParams<Params = HNil>,
        // For an ungrouped query `ValidSelect` requires a homogeneous projection (no mixing of a
        // bare column and an aggregate) with compatible ordering; a `GROUP BY` lifts that.
        <Self as SelectAst<'conn, 'scope, Conn>>::Grouped:
            crate::ValidSelect<P, <Self as SelectAst<'conn, 'scope, Conn>>::OrderClass>,
        <P as ReturningProjection<'scope>>::Shape: ProjectionShape,
        <<P as ReturningProjection<'scope>>::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
    {
        let exprs = self.exprs();
        let projection = projection(exprs.to_tuple());
        let selected = Selected::<'scope, _, <P as ReturningProjection<'scope>>::Shape, _>::new(
            self, projection,
        );
        let connection = selected.connection::<Conn>();
        <<Conn as QueryBuilder>::Select<
            'conn,
            'scope,
            Self,
            <P as ReturningProjection<'scope>>::Shape,
            P,
        > as SelectQuery<'conn, 'scope, Self, P>>::build_selected(connection, selected)
    }

    /// Like [`select`](Self::select), but returns the backend-neutral [`Selected`] directly instead of
    /// wrapping it in the backend's executable query type. View definitions use this so their body can
    /// be lowered into the schema model without a real backend; it enforces the same projection
    /// validity rules as `select`.
    fn project<P>(
        self,
        projection: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> P,
    ) -> Selected<'scope, Self, <P as ReturningProjection<'scope>>::Shape, P>
    where
        P: ReturningProjection<'scope> + Projectable,
        <Self as SelectAst<'conn, 'scope, Conn>>::Grouped:
            crate::ValidSelect<P, <Self as SelectAst<'conn, 'scope, Conn>>::OrderClass>,
        <P as ReturningProjection<'scope>>::Shape: ProjectionShape,
    {
        let exprs = self.exprs();
        let projection = projection(exprs.to_tuple());
        Selected::<'scope, _, <P as ReturningProjection<'scope>>::Shape, _>::new(self, projection)
    }

    /// Like [`select`](Self::select), but the projection closure also receives a [`Subqueries`]
    /// handle for projecting scalar subqueries (`SELECT (SELECT …)`), which may be correlated.
    ///
    /// A projected scalar subquery renders before the outer `FROM`, so a runtime [`param`] inside it
    /// would emit a placeholder the top-level query can't bind (its executable/prepared param shape
    /// is the source chain's). To stop that from silently producing an unbound placeholder, the
    /// projection must be free of runtime params (`P: ProjectionParams<Params = HNil>`). A correlated
    /// scalar subquery referencing outer *columns* — the common case — carries none.
    fn select_correlated<P>(
        self,
        projection: impl FnOnce(<Self::Exprs as ToTuple>::Tuple, Subqueries<'conn, 'scope, Conn>) -> P,
    ) -> Conn::Select<'conn, 'scope, Self, <P as ReturningProjection<'scope>>::Shape, P>
    where
        P: ReturningProjection<'scope> + Projectable + crate::ProjectionParams<Params = HNil>,
        <Self as SelectAst<'conn, 'scope, Conn>>::Grouped:
            crate::ValidSelect<P, <Self as SelectAst<'conn, 'scope, Conn>>::OrderClass>,
        <P as ReturningProjection<'scope>>::Shape: ProjectionShape,
        <<P as ReturningProjection<'scope>>::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
    {
        let subqueries = Subqueries::new(self.connection(), self.depth() + 1);
        let exprs = self.exprs();
        let projection = projection(exprs.to_tuple(), subqueries);
        let selected = Selected::<'scope, _, <P as ReturningProjection<'scope>>::Shape, _>::new(
            self, projection,
        );
        let connection = selected.connection::<Conn>();
        <<Conn as QueryBuilder>::Select<
            'conn,
            'scope,
            Self,
            <P as ReturningProjection<'scope>>::Shape,
            P,
        > as SelectQuery<'conn, 'scope, Self, P>>::build_selected(connection, selected)
    }
}

impl<'conn, 'scope, Conn, Query> SourceQuery<'conn, 'scope, Conn> for Query
where
    Conn: QueryBuilder + 'conn,
    Query: SelectAst<'conn, 'scope, Conn>,
{
}

impl<Base, S> JoinTarget<Base, S>
where
    S: QuerySource,
{
    pub fn on<'conn, 'scope, Conn, P, PredicateAst>(
        self,
        on: impl FnOnce(
            <Base::Exprs as ToTuple>::Tuple,
            <S as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope, P, PredicateAst>,
    ) -> Join<
        Base,
        <S as ProjectionShape>::Exprs<'scope>,
        InnerJoinSource<'scope, S, P, PredicateAst>,
    >
    where
        Conn: QueryBuilder + 'conn,
        Base: SelectAst<'conn, 'scope, Conn>,
        P: PredicateKind,
        PredicateAst: crate::PredicateAst + crate::NonAggregatePredicate,
        <S as ProjectionShape>::Exprs<'scope>: Clone,
    {
        let alias = SourceAlias::new(self.base.depth(), Base::Exprs::LEN);
        let right = S::exprs(alias);
        let join_on = on(self.base.exprs().to_tuple(), right.clone());
        Join {
            base: self.base,
            expr: right,
            source: InnerJoinSource::new(alias, join_on),
        }
    }
}

impl<Base, S> LeftJoinTarget<Base, S>
where
    S: QuerySource,
    Maybe<S>: ProjectionShape,
{
    pub fn on<'conn, 'scope, Conn, P, PredicateAst>(
        self,
        on: impl FnOnce(
            <Base::Exprs as ToTuple>::Tuple,
            <S as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope, P, PredicateAst>,
    ) -> LeftJoin<
        Base,
        <Maybe<S> as ProjectionShape>::Exprs<'scope>,
        LeftJoinSource<'scope, S, P, PredicateAst>,
    >
    where
        Conn: QueryBuilder + 'conn,
        Base: SelectAst<'conn, 'scope, Conn>,
        P: PredicateKind,
        PredicateAst: crate::PredicateAst + crate::NonAggregatePredicate,
    {
        let alias = SourceAlias::new(self.base.depth(), Base::Exprs::LEN);
        let joined = S::exprs(alias);
        let projection = Maybe::<S>::exprs(alias);
        let join_on = on(self.base.exprs().to_tuple(), joined);
        LeftJoin {
            base: self.base,
            expr: projection,
            source: LeftJoinSource::new(alias, join_on),
        }
    }
}

impl<Base, S> RightJoinTarget<Base, S>
where
    S: QuerySource,
{
    pub fn on<'conn, 'scope, Conn, P, PredicateAst>(
        self,
        on: impl FnOnce(
            <Base::Exprs as ToTuple>::Tuple,
            <S as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope, P, PredicateAst>,
    ) -> OuterJoin<
        Base,
        <S as ProjectionShape>::Exprs<'scope>,
        RightJoinSource<'scope, S, P, PredicateAst>,
    >
    where
        Conn: QueryBuilder + 'conn,
        Base: SelectAst<'conn, 'scope, Conn>,
        P: PredicateKind,
        PredicateAst: crate::PredicateAst + crate::NonAggregatePredicate,
        <S as ProjectionShape>::Exprs<'scope>: Clone,
    {
        // The joined table stays non-null (like an inner join); the accumulated base becomes nullable
        // when `OuterJoin::exprs` nullable-wraps it. The ON predicate references columns by alias, so
        // it sees the non-null views (nullability only changes the projected/decoded shape).
        let alias = SourceAlias::new(self.base.depth(), Base::Exprs::LEN);
        let right = S::exprs(alias);
        let join_on = on(self.base.exprs().to_tuple(), right.clone());
        OuterJoin {
            base: self.base,
            expr: right,
            source: RightJoinSource::new(alias, join_on),
        }
    }
}

impl<Base, S> FullJoinTarget<Base, S>
where
    S: QuerySource,
    Maybe<S>: ProjectionShape,
{
    pub fn on<'conn, 'scope, Conn, P, PredicateAst>(
        self,
        on: impl FnOnce(
            <Base::Exprs as ToTuple>::Tuple,
            <S as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope, P, PredicateAst>,
    ) -> OuterJoin<
        Base,
        <Maybe<S> as ProjectionShape>::Exprs<'scope>,
        FullJoinSource<'scope, S, P, PredicateAst>,
    >
    where
        Conn: QueryBuilder + 'conn,
        Base: SelectAst<'conn, 'scope, Conn>,
        P: PredicateKind,
        PredicateAst: crate::PredicateAst + crate::NonAggregatePredicate,
    {
        // Both sides nullable: the new table is projected via `Maybe<S>` and the accumulated base is
        // nullable-wrapped by `OuterJoin::exprs`. (`full_join` is gated to `SupportsFullJoin` backends.)
        let alias = SourceAlias::new(self.base.depth(), Base::Exprs::LEN);
        let joined = S::exprs(alias);
        let projection = Maybe::<S>::exprs(alias);
        let join_on = on(self.base.exprs().to_tuple(), joined);
        OuterJoin {
            base: self.base,
            expr: projection,
            source: FullJoinSource::new(alias, join_on),
        }
    }
}

#[doc(hidden)]
pub trait DeleteSourceAst<'conn, 'scope, Conn>
where
    Conn: QueryBuilder,
{
    type Table: TableProjection;
    type Filters: PredicateNodes;

    fn into_delete_parts(self) -> (&'conn Conn, usize, Self::Filters);
}

impl<'conn, 'scope, Conn, S> DeleteSourceAst<'conn, 'scope, Conn>
    for From<'conn, 'scope, Conn, HCons<<S as ProjectionShape>::Exprs<'scope>, HNil>, RootSource<S>>
where
    Conn: QueryBuilder + 'conn,
    S: QuerySource,
{
    type Table = S;
    type Filters = HNil;

    fn into_delete_parts(self) -> (&'conn Conn, usize, Self::Filters) {
        (self.connection, self.depth, HNil)
    }
}

impl<'conn, 'scope, Conn, Base, P, PredicateAst> DeleteSourceAst<'conn, 'scope, Conn>
    for Where<'scope, Base, P, PredicateAst>
where
    Conn: QueryBuilder + 'conn,
    Base: DeleteSourceAst<'conn, 'scope, Conn>,
    Base::Filters: PushBack<Predicate<'scope, P, PredicateAst>>,
    <Base::Filters as PushBack<Predicate<'scope, P, PredicateAst>>>::Output: PredicateNodes,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    type Table = Base::Table;
    type Filters = <Base::Filters as PushBack<Predicate<'scope, P, PredicateAst>>>::Output;

    fn into_delete_parts(self) -> (&'conn Conn, usize, Self::Filters) {
        let (connection, depth, filters) = self.base.into_delete_parts();
        (connection, depth, filters.push_back(self.predicate))
    }
}

impl<'conn, 'scope, Conn, Base> DeleteSourceAst<'conn, 'scope, Conn> for AllRows<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: DeleteSourceAst<'conn, 'scope, Conn>,
{
    type Table = Base::Table;
    type Filters = Base::Filters;

    fn into_delete_parts(self) -> (&'conn Conn, usize, Self::Filters) {
        self.base.into_delete_parts()
    }
}

pub trait DeleteSourceQuery<'conn, 'scope, Conn>:
    DeleteSourceAst<'conn, 'scope, Conn> + Sized
where
    Conn: QueryBuilder + 'conn,
{
    fn delete(self) -> impl Future<Output = Result<u64, ErrorOf<Conn>>> + Send + 'conn
    where
        Conn: Connection + 'conn,
        'scope: 'conn,
        Self: 'conn,
        // A view is read-only: it does not implement `UpdateableTable`, so deleting through one is a
        // compile error rather than a `DELETE` that would mutate base tables or fail at runtime.
        Self::Table: UpdateableTable + 'conn,
        Self::Filters: PredicateNodes,
        <Self::Filters as PredicateNodes>::Params: NoRuntimeParams,
        // See `insert`: the future captures the query object, so require it `Send`.
        <Conn as QueryBuilder>::Delete<'conn, Self::Table, (), Self::Filters, ()>:
            ExecutableDeleteQuery<'conn, Self::Filters, ()> + Send,
    {
        let (connection, depth, filters) = self.into_delete_parts();
        let alias = SourceAlias::new(depth, 0);
        let query = <<Conn as QueryBuilder>::Delete<
            'conn,
            Self::Table,
            (),
            Self::Filters,
            (),
        > as DeleteQuery<'conn, Self::Filters, ()>>::build(connection, alias, filters, ());
        async move { ExecutableDeleteQuery::execute(&query).await }
    }

    fn delete_returning<P>(
        self,
        projection: impl FnOnce(<Self::Table as ProjectionShape>::Exprs<'scope>) -> P,
    ) -> Conn::Delete<'conn, Self::Table, <P as ReturningProjection<'scope>>::Shape, Self::Filters, P>
    where
        // Read-only views do not implement `UpdateableTable`, so they cannot be deleted from.
        Self::Table: UpdateableTable + 'conn,
        // Aggregates are never valid in `RETURNING`, so require an aggregate-free projection.
        P: ReturningProjection<'scope>
            + Projectable
            + crate::ProjectionClass<Class = crate::ScalarProjection>
            // Window functions are invalid in a RETURNING clause; `ReturnableProjection` excludes them
            + crate::ReturnableProjection
            + crate::ProjectionParams<Params = HNil>,
        <P::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
        Conn::Backend: SupportsReturning,
    {
        let (connection, depth, filters) = self.into_delete_parts();
        let alias = SourceAlias::new(depth, 0);
        let table = <Self::Table as ProjectionShape>::exprs(alias);
        let projection = projection(table);
        <<Conn as QueryBuilder>::Delete<
            'conn,
            Self::Table,
            <P as ReturningProjection<'scope>>::Shape,
            Self::Filters,
            P,
        > as DeleteQuery<'conn, Self::Filters, P>>::build(
            connection, alias, filters, projection
        )
    }
}

impl<'conn, 'scope, Conn, Base, P, PredicateAst> DeleteSourceQuery<'conn, 'scope, Conn>
    for Where<'scope, Base, P, PredicateAst>
where
    Conn: QueryBuilder + 'conn,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
    Where<'scope, Base, P, PredicateAst>: DeleteSourceAst<'conn, 'scope, Conn>,
{
}

impl<'conn, 'scope, Conn, Base> DeleteSourceQuery<'conn, 'scope, Conn> for AllRows<Base>
where
    Conn: QueryBuilder + 'conn,
    AllRows<Base>: DeleteSourceAst<'conn, 'scope, Conn>,
{
}

thread_local! {
    static QUERY_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct QueryDepthReset<'depth> {
    depth: &'depth Cell<usize>,
    previous: usize,
}

impl Drop for QueryDepthReset<'_> {
    fn drop(&mut self) {
        self.depth.set(self.previous);
    }
}

fn with_next_query_depth<R>(f: impl FnOnce(usize) -> R) -> R {
    QUERY_DEPTH.with(|depth| {
        let previous = depth.get();
        depth.set(previous + 1);
        let _reset = QueryDepthReset { depth, previous };
        f(previous)
    })
}

/// Build a typed sourceless select from a projection value.
pub(crate) fn build_sourceless_select<'conn, Conn, Projection>(
    connection: &'conn Conn,
    projection: Projection,
) -> Conn::Select<
    'conn,
    'static,
    NoSources<'conn, Conn>,
    <Projection as ReturningProjection<'static>>::Shape,
    Projection,
>
where
    Conn: QueryBuilder + 'conn,
    Projection: ReturningProjection<'static> + Projectable,
    <Projection as ReturningProjection<'static>>::Shape: ProjectionShape,
    <<Projection as ReturningProjection<'static>>::Shape as ProjectionShape>::Row:
        Decode<Conn::Backend>,
{
    with_next_query_depth(|current_depth| {
        let select = Select::new(
            NoSources::new(connection, current_depth),
            NoFilters,
            NoOrdering,
            projection,
        );
        let selected =
            select.into_selected::<<Projection as ReturningProjection<'static>>::Shape>();

        <<Conn as QueryBuilder>::Select<
            'conn,
            'static,
            NoSources<'conn, Conn>,
            <Projection as ReturningProjection<'static>>::Shape,
            Projection,
        > as SelectQuery<'conn, 'static, NoSources<'conn, Conn>, Projection>>::build_selected(
            connection, selected,
        )
    })
}

/// Construct the initial consuming source-first select builder.
pub(crate) fn build_from_builder<'conn, Conn, S>(
    connection: &'conn Conn,
) -> From<'conn, 'conn, Conn, HCons<<S as ProjectionShape>::Exprs<'conn>, HNil>, RootSource<S>>
where
    Conn: QueryBuilder + 'conn,
    S: QuerySource,
{
    with_next_query_depth(|current_depth| From::new(connection, current_depth))
}
