use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::marker::PhantomData;
use std::pin::pin;

use futures_core::Stream;

use crate::{
    Backend, ColumnExprAst, ColumnRef, Connection, Decode, Expr, ExprAst, ExprKind, HCons, HList,
    HNil, InsertableTable, Maybe, NoRuntimeParams, Order, ParamExprAst, Predicate, PredicateKind,
    Projectable, ProjectionShape, PushBack, QueryBuilder, RenderAst, RenderProjectable,
    RuntimeParam, SourceAlias, SupportsReturning, TableProjection, ToTuple, UpdateableTable,
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
}

// Native `uuid` column support: a bare `uuid::Uuid` value can be assigned to a nullable UUID column
// (`.col(id)`). `Some(id)` / `None` already route through the generic `Option<T>` impls below.
#[cfg(feature = "uuid")]
impl_nullable_assignment_value! { uuid::Uuid }

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
    Ast: ExprAst,
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
    Ast: ExprAst,
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

    pub fn insert(self) -> impl Future<Output = Result<u64, ErrorOf<Conn>>> + 'conn
    where
        Conn: Connection + 'conn,
        S: 'conn,
        Rows: NonEmptyInsertRows + 'conn,
        Rows::Params: NoRuntimeParams,
        <Conn as QueryBuilder>::Insert<'conn, S, (), Rows, ()>:
            ExecutableInsertQuery<'conn, Rows, ()>,
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
        P: ReturningProjection<'static> + Projectable,
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
        PredicateAst: crate::PredicateAst,
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
    pub fn update(self) -> impl Future<Output = Result<u64, ErrorOf<Conn>>> + 'conn
    where
        Conn: Connection + 'conn,
        Columns::Params: NoRuntimeParams,
        Filters::Params: NoRuntimeParams,
        <Conn as QueryBuilder>::Update<'conn, S, (), Columns, Filters, ()>:
            ExecutableUpdateQuery<'conn, Columns, Filters, ()>,
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
        P: ReturningProjection<'static> + Projectable,
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

    fn push_filter<P, PredicateAst>(
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
}

#[doc(hidden)]
pub trait SourceSpec {
    type Params: HList;
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
    S: TableProjection,
{
    type Params = HNil;
}

impl<S, B> RenderSourceSpec<B> for RootSource<S>
where
    S: TableProjection,
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
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    type Params = PredicateAst::Params;
}

impl<S, P, PredicateAst, B> RenderSourceSpec<B> for InnerJoinSource<'_, S, P, PredicateAst>
where
    S: TableProjection,
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
    S: TableProjection,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
{
    type Params = PredicateAst::Params;
}

impl<S, P, PredicateAst, B> RenderSourceSpec<B> for LeftJoinSource<'_, S, P, PredicateAst>
where
    S: TableProjection,
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

    #[doc(hidden)]
    pub fn lower_into<'conn, Conn, Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Conn: QueryBuilder + 'conn,
        Base: RenderSelectAst<'conn, 'scope, Conn, Sink::Backend>,
        Sink: SelectSink,
        Projection: RenderProjectable<Sink::Backend>,
    {
        sink.push_projection::<Shape, _>(self.projection.clone())?;
        self.base.lower_sources_into(sink)?;
        self.base.lower_filters_into(sink)?;
        self.base.lower_orders_into(sink)?;
        self.base.lower_bounds_into(sink)
    }
}

#[doc(hidden)]
pub trait SelectAst<'conn, 'scope, Conn>
where
    Conn: QueryBuilder,
{
    type Exprs: HList + Clone + ToTuple;
    type Params: HList;

    fn depth(&self) -> usize;

    fn connection(&self) -> &'conn Conn;

    fn exprs(&self) -> Self::Exprs;
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

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>;

    fn lower_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink<Backend = B>,
    {
        self.lower_sources_into(sink)?;
        self.lower_filters_into(sink)?;
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

    fn depth(&self) -> usize {
        self.depth
    }

    fn connection(&self) -> &'conn Conn {
        self.connection
    }

    fn exprs(&self) -> Self::Exprs {
        HNil
    }
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
    S: TableProjection,
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
        PredicateAst: crate::PredicateAst,
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

    fn depth(&self) -> usize {
        self.depth
    }

    fn connection(&self) -> &'conn Conn {
        self.connection
    }

    fn exprs(&self) -> Self::Exprs {
        self.exprs.clone()
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
pub struct Limited<Base> {
    base: Base,
    rows: usize,
}

#[doc(hidden)]
pub struct Offset<Base> {
    base: Base,
    rows: usize,
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

impl<'conn, 'scope, Conn, Base, P, PredicateAst> SelectAst<'conn, 'scope, Conn>
    for Where<'scope, Base, P, PredicateAst>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    P: PredicateKind,
    PredicateAst: crate::PredicateAst,
    Base::Params: crate::HAppend<PredicateAst::Params>,
{
    type Exprs = Base::Exprs;
    type Params = <Base::Params as crate::HAppend<PredicateAst::Params>>::Output;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }
}

impl<'conn, 'scope, Conn, Base, P, PredicateAst, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for Where<'scope, Base, P, PredicateAst>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    P: PredicateKind,
    PredicateAst: crate::RenderPredicateAst<B>,
    Base::Params: crate::HAppend<PredicateAst::Params>,
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
}

impl<'conn, 'scope, Conn, Base, K, Ast> SelectAst<'conn, 'scope, Conn>
    for OrderBy<'scope, Base, K, Ast>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    K: ExprKind,
    Ast: ExprAst,
    Base::Params: crate::HAppend<Ast::Params>,
{
    type Exprs = Base::Exprs;
    type Params = <Base::Params as crate::HAppend<Ast::Params>>::Output;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }
}

impl<'conn, 'scope, Conn, Base, K, Ast, B> RenderSelectAst<'conn, 'scope, Conn, B>
    for OrderBy<'scope, Base, K, Ast>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    K: ExprKind,
    Ast: RenderAst<B>,
    Base::Params: crate::HAppend<Ast::Params>,
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
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for Limited<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Params = Base::Params;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
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
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for Offset<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Params = Base::Params;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
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
    Base::Params: crate::HAppend<Source::Params>,
{
    type Exprs = <Base::Exprs as PushBack<Expr>>::Output;
    type Params = <Base::Params as crate::HAppend<Source::Params>>::Output;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs().push_back(self.expr.clone())
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
    Base::Params: crate::HAppend<Source::Params>,
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
    Base::Params: crate::HAppend<Source::Params>,
{
    type Exprs = <Base::Exprs as PushBack<Expr>>::Output;
    type Params = <Base::Params as crate::HAppend<Source::Params>>::Output;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn connection(&self) -> &'conn Conn {
        self.base.connection()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs().push_back(self.expr.clone())
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
    Base::Params: crate::HAppend<Source::Params>,
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
        PredicateAst: crate::PredicateAst,
    {
        let predicate = predicate(self.exprs().to_tuple());
        Where {
            base: self,
            predicate,
        }
    }

    fn order_by<K, Ast>(
        self,
        order: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> Order<'scope, K, Ast>,
    ) -> OrderBy<'scope, Self, K, Ast>
    where
        K: ExprKind,
        Ast: ExprAst,
    {
        let order = order(self.exprs().to_tuple());
        OrderBy { base: self, order }
    }

    fn limit(self, rows: usize) -> Limited<Self> {
        Limited { base: self, rows }
    }

    fn offset(self, rows: usize) -> Offset<Self> {
        Offset { base: self, rows }
    }

    fn join<S>(self) -> JoinTarget<Self, S>
    where
        S: TableProjection,
    {
        JoinTarget {
            base: self,
            _source: PhantomData,
        }
    }

    fn left_join<S>(self) -> LeftJoinTarget<Self, S>
    where
        S: TableProjection,
    {
        LeftJoinTarget {
            base: self,
            _source: PhantomData,
        }
    }

    fn select<P>(
        self,
        projection: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> P,
    ) -> Conn::Select<'conn, 'scope, Self, <P as ReturningProjection<'scope>>::Shape, P>
    where
        P: ReturningProjection<'scope> + Projectable,
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
}

impl<'conn, 'scope, Conn, Query> SourceQuery<'conn, 'scope, Conn> for Query
where
    Conn: QueryBuilder + 'conn,
    Query: SelectAst<'conn, 'scope, Conn>,
{
}

impl<Base, S> JoinTarget<Base, S>
where
    S: TableProjection,
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
        PredicateAst: crate::PredicateAst,
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
    S: TableProjection,
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
        PredicateAst: crate::PredicateAst,
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
    S: TableProjection,
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
    fn delete(self) -> impl Future<Output = Result<u64, ErrorOf<Conn>>> + 'conn
    where
        Conn: Connection + 'conn,
        'scope: 'conn,
        Self: 'conn,
        Self::Table: 'conn,
        Self::Filters: PredicateNodes,
        <Self::Filters as PredicateNodes>::Params: NoRuntimeParams,
        <Conn as QueryBuilder>::Delete<'conn, Self::Table, (), Self::Filters, ()>:
            ExecutableDeleteQuery<'conn, Self::Filters, ()>,
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
        Self::Table: 'conn,
        P: ReturningProjection<'scope> + Projectable,
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
    S: TableProjection,
{
    with_next_query_depth(|current_depth| From::new(connection, current_depth))
}
