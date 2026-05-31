use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::marker::PhantomData;
use std::pin::pin;

use futures_core::Stream;

use crate::{
    Backend, BindValue, ColumnRef, Connection, Decode, Expr, ExprAst, ExprKind, HCons, HList, HNil,
    InsertableTable, IntoBindValue, IntoNullableBindValue, Maybe, NoRuntimeParams, Order,
    ParamExprAst, Predicate, PredicateKind, Projectable, ProjectionShape, PushBack, QueryBuilder,
    RuntimeParam, SourceAlias, TableProjection, ToTuple, UpdateableTable,
};

type ErrorOf<Builder> = <<Builder as QueryBuilder>::Backend as Backend>::Error;

/// Type-level identity for a table column that can be assigned in mutations.
#[doc(hidden)]
pub trait ColumnKey: ExprKind {
    const NAME: &'static str;
}

/// A typed insert assignment for a single generated table column.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct InsertAssignment<K, Value = StaticAssignmentValue>
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
pub struct UpdateAssignment<K, Value = StaticAssignmentValue>
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
pub struct StaticAssignmentValue {
    value: BindValue,
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AssignmentValueRef<'value> {
    Static(&'value BindValue),
    Runtime,
}

#[doc(hidden)]
pub trait AssignmentValueNode: Clone {
    type Params: HList;

    fn as_ref(&self) -> AssignmentValueRef<'_>;
}

impl StaticAssignmentValue {
    pub fn new(value: BindValue) -> Self {
        Self { value }
    }
}

impl std::convert::From<BindValue> for StaticAssignmentValue {
    fn from(value: BindValue) -> Self {
        Self::new(value)
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

impl<K> Clone for RuntimeAssignmentValue<K>
where
    K: ExprKind,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for RuntimeAssignmentValue<K> where K: ExprKind {}

impl AssignmentValueNode for StaticAssignmentValue {
    type Params = HNil;

    fn as_ref(&self) -> AssignmentValueRef<'_> {
        AssignmentValueRef::Static(&self.value)
    }
}

impl<K> AssignmentValueNode for RuntimeAssignmentValue<K>
where
    K: ExprKind,
{
    type Params = HCons<K::Value, HNil>;

    fn as_ref(&self) -> AssignmentValueRef<'_> {
        AssignmentValueRef::Runtime
    }
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

impl<K, Value> IntoAssignmentValue<K> for Value
where
    K: ColumnKey,
    Value: IntoBindValue,
{
    type Value = StaticAssignmentValue;

    fn into_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(self.into_bind_value())
    }
}

macro_rules! impl_nullable_assignment_value {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<K> IntoNullableAssignmentValue<K> for $ty
            where
                K: ColumnKey<Value = $ty>,
            {
                type Value = StaticAssignmentValue;

                fn into_nullable_assignment_value(self) -> Self::Value {
                    StaticAssignmentValue::new(self.into_nullable_bind_value())
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

impl<K> IntoNullableAssignmentValue<K> for &str
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue;

    fn into_nullable_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(
            <&str as IntoNullableBindValue<String>>::into_nullable_bind_value(self),
        )
    }
}

impl<K> IntoNullableAssignmentValue<K> for &String
where
    K: ColumnKey<Value = String>,
{
    type Value = StaticAssignmentValue;

    fn into_nullable_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(
            <&String as IntoNullableBindValue<String>>::into_nullable_bind_value(self),
        )
    }
}

impl<K, T> IntoNullableAssignmentValue<K> for Option<T>
where
    K: ColumnKey<Value = T>,
    T: IntoBindValue,
{
    type Value = StaticAssignmentValue;

    fn into_nullable_assignment_value(self) -> Self::Value {
        StaticAssignmentValue::new(
            <Option<T> as IntoNullableBindValue<T>>::into_nullable_bind_value(self),
        )
    }
}

impl<'scope, K> IntoAssignmentValue<K> for Expr<'scope, RuntimeParam<K>, ParamExprAst<K>>
where
    K: ColumnKey,
{
    type Value = RuntimeAssignmentValue<K>;

    fn into_assignment_value(self) -> Self::Value {
        RuntimeAssignmentValue::new()
    }
}

impl<'scope, K> IntoNullableAssignmentValue<K> for Expr<'scope, RuntimeParam<K>, ParamExprAst<K>>
where
    K: ColumnKey,
{
    type Value = RuntimeAssignmentValue<K>;

    fn into_nullable_assignment_value(self) -> Self::Value {
        RuntimeAssignmentValue::new()
    }
}

#[doc(hidden)]
pub trait InsertAssignmentNode {
    type Params: HList;

    fn column(&self) -> &'static str;

    fn value(&self) -> AssignmentValueRef<'_>;
}

impl<K, Value> InsertAssignmentNode for InsertAssignment<K, Value>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
    type Params = Value::Params;

    fn column(&self) -> &'static str {
        K::NAME
    }

    fn value(&self) -> AssignmentValueRef<'_> {
        self.value.as_ref()
    }
}

#[doc(hidden)]
pub trait UpdateAssignmentNode {
    type Params: HList;

    fn column(&self) -> &'static str;

    fn value(&self) -> AssignmentValueRef<'_>;
}

impl<K, Value> UpdateAssignmentNode for UpdateAssignment<K, Value>
where
    K: ColumnKey,
    Value: AssignmentValueNode,
{
    type Params = Value::Params;

    fn column(&self) -> &'static str {
        K::NAME
    }

    fn value(&self) -> AssignmentValueRef<'_> {
        self.value.as_ref()
    }
}

/// Heterogeneous list of typed insert assignments.
#[doc(hidden)]
pub trait InsertAssignments {
    type Params: HList;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn try_for_each<E>(
        &self,
        f: impl FnMut(&'static str, AssignmentValueRef<'_>) -> Result<(), E>,
    ) -> Result<(), E>;
}

impl InsertAssignments for HNil {
    type Params = HNil;

    fn len(&self) -> usize {
        0
    }

    fn try_for_each<E>(
        &self,
        _f: impl FnMut(&'static str, AssignmentValueRef<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
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

    fn try_for_each<E>(
        &self,
        mut f: impl FnMut(&'static str, AssignmentValueRef<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        f(self.head.column(), self.head.value())?;
        self.tail.try_for_each(f)
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

    fn try_for_each<E>(
        &self,
        f: impl FnMut(&'static str, AssignmentValueRef<'_>) -> Result<(), E>,
    ) -> Result<(), E>;
}

impl UpdateAssignments for HNil {
    type Params = HNil;

    fn len(&self) -> usize {
        0
    }

    fn try_for_each<E>(
        &self,
        _f: impl FnMut(&'static str, AssignmentValueRef<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
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

    fn try_for_each<E>(
        &self,
        mut f: impl FnMut(&'static str, AssignmentValueRef<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        f(self.head.column(), self.head.value())?;
        self.tail.try_for_each(f)
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

    fn try_visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateVisitor;
}

#[doc(hidden)]
pub trait PredicateVisitor {
    type Error;

    fn visit_predicate<Kind, Ast>(
        &mut self,
        predicate: &Predicate<'_, Kind, Ast>,
    ) -> Result<(), Self::Error>
    where
        Kind: PredicateKind,
        Ast: crate::PredicateAst;
}

impl PredicateNodes for HNil {
    type Params = HNil;

    fn len(&self) -> usize {
        0
    }

    fn try_visit<V>(&self, _visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateVisitor,
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

    fn try_visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateVisitor,
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
        ParamValues: crate::PreparedParamValues<Self::Params>;

    fn collect<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<Vec<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
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
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
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
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
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
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query;

    fn fetch<'query, ParamValues>(&'query self, params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues: crate::PreparedParamValues<Self::Params>;

    fn collect<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<Vec<Self::Row>, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
    {
        let rows = self.fetch(params);
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query, ParamValues>(
        &'query self,
        params: ParamValues,
    ) -> impl Future<Output = Result<(Vec<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
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
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
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
    ) -> impl Future<Output = Result<(Option<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
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
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
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
        ParamValues: crate::PreparedParamValues<Self::Params> + 'query,
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
pub trait InsertQuery<'builder, Columns, Returning>
where
    Columns: InsertAssignments,
    Returning: Projectable,
{
    type Builder: QueryBuilder + 'builder;
    type Table: InsertableTable;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn build(builder: &'builder Self::Builder, columns: Columns, returning: Returning) -> Self
    where
        Self: Sized;
}

/// An insert query object that can execute or fetch rows through a connection.
pub trait ExecutableInsertQuery<'conn, Columns, Returning>:
    InsertQuery<'conn, Columns, Returning>
where
    Self::Builder: Connection,
    Columns: InsertAssignments,
    Columns::Params: NoRuntimeParams,
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
        Returning: 'query,
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Vec<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Columns: 'query,
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
    ) -> impl Future<Output = Result<(Option<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
        Columns: 'query,
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
        Returning: 'query,
    {
        let rows = self.fetch();
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// An insert query object that can be compiled into a backend-owned prepared statement.
pub trait PreparableInsertQuery<'conn, Columns, Returning>:
    InsertQuery<'conn, Columns, Returning>
where
    Self::Builder: Connection,
    Columns: InsertAssignments,
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
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<Output = Result<Self::Prepared<'prepared>, ErrorOf<Self::Builder>>> + 'prepared
    where
        'conn: 'prepared,
        Columns: 'prepared,
        Returning: 'prepared;
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
    ) -> impl Future<Output = Result<(Vec<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
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
    ) -> impl Future<Output = Result<(Option<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
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
    ) -> impl Future<Output = Result<(Vec<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
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
    ) -> impl Future<Output = Result<(Option<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
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
    T: ExprKind + ProjectionShape + IntoBindValue + Clone,
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

    fn push_projection<Shape, P>(&mut self, projection: P) -> Result<(), Self::Error>
    where
        Shape: ProjectionShape,
        P: Projectable;

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
        PredicateAst: crate::PredicateAst;

    fn push_left_join<S, P, PredicateAst>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, PredicateAst>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        PredicateAst: crate::PredicateAst;

    fn push_filter<P, PredicateAst>(
        &mut self,
        predicate: Predicate<'_, P, PredicateAst>,
    ) -> Result<(), Self::Error>
    where
        P: PredicateKind,
        PredicateAst: crate::PredicateAst;

    fn push_order<K, Ast>(&mut self, order: Order<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: ExprAst;

    fn set_limit(&mut self, rows: usize) -> Result<(), Self::Error>;

    fn set_offset(&mut self, rows: usize) -> Result<(), Self::Error>;
}

#[doc(hidden)]
pub trait SourceSpec {
    type Params: HList;

    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink;
}

impl<S> SourceSpec for RootSource<S>
where
    S: TableProjection,
{
    type Params = HNil;

    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn push_source<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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
        Base: SelectAst<'conn, 'scope, Conn>,
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink;

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink;

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink;

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink;

    fn lower_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        Ok(())
    }

    fn lower_filters_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        Ok(())
    }

    fn lower_orders_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        Ok(())
    }

    fn lower_bounds_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.source.push_source(sink)
    }

    fn lower_filters_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        Ok(())
    }

    fn lower_orders_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        Ok(())
    }

    fn lower_bounds_into<Sink>(&self, _sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_filters_into(sink)?;
        sink.push_filter(self.predicate.clone())
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_orders_into(sink)?;
        sink.push_order(self.order.clone())
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_sources_into(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_sources_into(sink)?;
        self.source.push_source(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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

    fn lower_sources_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_sources_into(sink)?;
        self.source.push_source(sink)
    }

    fn lower_filters_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_filters_into(sink)
    }

    fn lower_orders_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
    {
        self.base.lower_orders_into(sink)
    }

    fn lower_bounds_into<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: SelectSink,
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
