use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::marker::PhantomData;

use futures_core::Stream;

use crate::ir::{
    Delete, Filter, Insert, InsertColumn, PredicateNode, Select, Sort, Source, Update, UpdateColumn,
};
use crate::{
    Backend, ColumnRef, Connection, Decode, Expr, ExprKind, HCons, HList, HNil, InsertableTable,
    IntoBindValue, IrList, Maybe, Order, Predicate, Projectable, ProjectionShape, PushBack,
    QueryBuilder, SchemaTable, SelectColumn, TableProjection, ToTuple, UpdateableTable,
};

type ErrorOf<Builder> = <<Builder as QueryBuilder>::Backend as Backend>::Error;

/// A row stream that can report affected rows after it is exhausted.
pub trait RowsAffected {
    fn rows_affected(&self) -> Option<u64>;
}

/// A backend-specific select query object backed by core-owned select IR.
pub trait SelectQuery<'builder> {
    type Builder: QueryBuilder + 'builder;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn ir(&self) -> &Select;

    fn build(builder: &'builder Self::Builder, select: Select) -> Self;
}

/// A select query object that can fetch rows through an executable connection.
pub trait ExecutableSelectQuery<'conn>: SelectQuery<'conn>
where
    Self::Builder: Connection,
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
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
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
    {
        let rows = self.fetch();
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// A backend-specific insert query object backed by core-owned insert IR.
pub trait InsertQuery<'builder> {
    type Builder: QueryBuilder + 'builder;
    type Table: InsertableTable;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn ir(&self) -> &Insert;

    fn build(builder: &'builder Self::Builder, insert: Insert) -> Self;
}

/// An insert query object that can execute or fetch rows through a connection.
pub trait ExecutableInsertQuery<'conn>: InsertQuery<'conn>
where
    Self::Builder: Connection,
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
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Vec<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
    {
        let rows = self.fetch();
        collect_rows_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Self::Row, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
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
    {
        let rows = self.fetch();
        fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
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
    {
        let rows = self.fetch();
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// A backend-specific update query object backed by core-owned update IR.
pub trait UpdateQuery<'builder> {
    type Builder: QueryBuilder + 'builder;
    type Table: UpdateableTable;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn ir(&self) -> &Update;

    fn build(builder: &'builder Self::Builder, update: Update) -> Self;
}

/// An update query object that can execute or fetch rows through a connection.
pub trait ExecutableUpdateQuery<'conn>: UpdateQuery<'conn>
where
    Self::Builder: Connection,
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
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Vec<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
    {
        let rows = self.fetch();
        collect_rows_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Self::Row, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
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
    {
        let rows = self.fetch();
        fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
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
    {
        let rows = self.fetch();
        fetch_optional_row::<Self::Builder, Self::Row, _>(rows)
    }
}

/// A backend-specific delete query object backed by core-owned delete IR.
pub trait DeleteQuery<'builder> {
    type Builder: QueryBuilder + 'builder;
    type Table: TableProjection;
    type Shape: ProjectionShape;
    type Row: Decode<<Self::Builder as QueryBuilder>::Backend> + Send;

    fn ir(&self) -> &Delete;

    fn build(builder: &'builder Self::Builder, delete: Delete) -> Self;
}

/// A delete query object that can execute or fetch rows through a connection.
pub trait ExecutableDeleteQuery<'conn>: DeleteQuery<'conn>
where
    Self::Builder: Connection,
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
    {
        let rows = self.fetch();
        collect_rows::<Self::Builder, Self::Row, _>(rows)
    }

    fn collect_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Vec<Self::Row>, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
    {
        let rows = self.fetch();
        collect_rows_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one_with_affected<'query>(
        &'query self,
    ) -> impl Future<Output = Result<(Self::Row, u64), ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
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
    {
        let rows = self.fetch();
        fetch_optional_row_with_affected::<Self::Builder, Self::Row, _>(rows)
    }

    fn fetch_one<'query>(
        &'query self,
    ) -> impl Future<Output = Result<Self::Row, ErrorOf<Self::Builder>>> + Send + 'query
    where
        'conn: 'query,
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
    let mut rows = Box::pin(rows);
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
    let mut rows = Box::pin(rows);
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
    let mut rows = Box::pin(rows);
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
    let mut rows = Box::pin(rows);
    poll_fn(|cx| rows.as_mut().poll_next(cx)).await.transpose()
}

/// A projection value that can identify the query shape returned by `returning`.
pub trait ReturningProjection<'scope>: Projectable {
    type Shape: ProjectionShape;
}

impl<'scope, K> ReturningProjection<'scope> for Expr<'scope, K>
where
    K: ExprKind + ProjectionShape,
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

/// A `returning` projection carrying the inferred query shape.
pub struct Returning<Shape, Columns>
where
    Shape: ProjectionShape,
    Columns: IrList<SelectColumn>,
{
    columns: Columns,
    _shape: PhantomData<Shape>,
}

impl<Shape, Columns> Returning<Shape, Columns>
where
    Shape: ProjectionShape,
    Columns: IrList<SelectColumn>,
{
    fn new(columns: Columns) -> Self {
        Self {
            columns,
            _shape: PhantomData,
        }
    }

    fn into_columns(self) -> Columns {
        self.columns
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct RootSource<S>
where
    S: TableProjection,
{
    alias: String,
    _phantom: PhantomData<S>,
}

impl<S> RootSource<S>
where
    S: TableProjection,
{
    fn new(alias: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct InnerJoinSource<S>
where
    S: TableProjection,
{
    alias: String,
    on: PredicateNode,
    _phantom: PhantomData<S>,
}

impl<S> InnerJoinSource<S>
where
    S: TableProjection,
{
    fn new(alias: impl Into<String>, on: PredicateNode) -> Self {
        Self {
            alias: alias.into(),
            on,
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct LeftJoinSource<S>
where
    S: TableProjection,
{
    alias: String,
    on: PredicateNode,
    _phantom: PhantomData<S>,
}

impl<S> LeftJoinSource<S>
where
    S: TableProjection,
{
    fn new(alias: impl Into<String>, on: PredicateNode) -> Self {
        Self {
            alias: alias.into(),
            on,
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
pub trait SourceSpec {
    fn into_source(self) -> Source;
}

impl<S> SourceSpec for RootSource<S>
where
    S: TableProjection,
{
    fn into_source(self) -> Source {
        Source::table(self.alias, S::qualified_name())
    }
}

impl<S> SourceSpec for InnerJoinSource<S>
where
    S: TableProjection,
{
    fn into_source(self) -> Source {
        Source::join(self.alias, S::qualified_name(), self.on)
    }
}

impl<S> SourceSpec for LeftJoinSource<S>
where
    S: TableProjection,
{
    fn into_source(self) -> Source {
        Source::left_join(self.alias, S::qualified_name(), self.on)
    }
}

#[doc(hidden)]
pub trait SourceSpecList {
    fn into_sources(self) -> Vec<Source>;
}

impl SourceSpecList for () {
    fn into_sources(self) -> Vec<Source> {
        Vec::new()
    }
}

impl SourceSpecList for HNil {
    fn into_sources(self) -> Vec<Source> {
        Vec::new()
    }
}

impl<Head, Tail> SourceSpecList for HCons<Head, Tail>
where
    Head: SourceSpec,
    Tail: SourceSpecList,
{
    fn into_sources(self) -> Vec<Source> {
        let mut sources = vec![self.head.into_source()];
        sources.extend(self.tail.into_sources());
        sources
    }
}

#[doc(hidden)]
pub struct SelectParts<'conn, Conn, Exprs, Sources>
where
    Conn: QueryBuilder,
    Exprs: HList,
    Sources: SourceSpecList,
{
    connection: &'conn Conn,
    depth: usize,
    exprs: Exprs,
    sources: Sources,
    filters: Vec<Filter>,
    orders: Vec<Sort>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[doc(hidden)]
pub trait SelectAst<'conn, 'scope, Conn>
where
    Conn: QueryBuilder,
{
    type Exprs: HList + Clone + ToTuple;
    type Sources: SourceSpecList;

    fn depth(&self) -> usize;

    fn exprs(&self) -> Self::Exprs;

    fn into_parts(self) -> SelectParts<'conn, Conn, Self::Exprs, Self::Sources>;
}

/// A consuming, source-first select builder carrying typed sources.
pub struct From<'conn, 'scope, Conn, Exprs, Sources>
where
    Conn: QueryBuilder,
    Exprs: HList,
    Sources: SourceSpecList,
{
    connection: &'conn Conn,
    depth: usize,
    exprs: Exprs,
    sources: Sources,
    _scope: PhantomData<&'scope ()>,
}

impl<'conn, 'scope, Conn, S>
    From<
        'conn,
        'scope,
        Conn,
        HCons<<S as ProjectionShape>::Exprs<'scope>, HNil>,
        HCons<RootSource<S>, HNil>,
    >
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection,
{
    pub(crate) fn new(connection: &'conn Conn, depth: usize) -> Self {
        let alias = format!("q{depth}_0");
        Self {
            connection,
            depth,
            exprs: HCons {
                head: S::exprs(&alias),
                tail: HNil,
            },
            sources: HCons {
                head: RootSource::new(alias),
                tail: HNil,
            },
            _scope: PhantomData,
        }
    }
}

impl<'conn, 'scope, Conn, Exprs, Sources> SelectAst<'conn, 'scope, Conn>
    for From<'conn, 'scope, Conn, Exprs, Sources>
where
    Conn: QueryBuilder + 'conn,
    Exprs: HList + Clone + ToTuple,
    Sources: SourceSpecList,
{
    type Exprs = Exprs;
    type Sources = Sources;

    fn depth(&self) -> usize {
        self.depth
    }

    fn exprs(&self) -> Self::Exprs {
        self.exprs.clone()
    }

    fn into_parts(self) -> SelectParts<'conn, Conn, Self::Exprs, Self::Sources> {
        SelectParts {
            connection: self.connection,
            depth: self.depth,
            exprs: self.exprs,
            sources: self.sources,
            filters: Vec::new(),
            orders: Vec::new(),
            limit: None,
            offset: None,
        }
    }
}

pub struct Where<Base> {
    base: Base,
    filter: Filter,
}

pub struct OrderBy<Base> {
    base: Base,
    order: Sort,
}

pub struct Limited<Base> {
    base: Base,
    rows: usize,
}

pub struct Offset<Base> {
    base: Base,
    rows: usize,
}

pub struct Join<Base, Expr, Source> {
    base: Base,
    expr: Expr,
    source: Source,
}

pub struct LeftJoin<Base, Expr, Source> {
    base: Base,
    expr: Expr,
    source: Source,
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for Where<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Sources = Base::Sources;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn into_parts(self) -> SelectParts<'conn, Conn, Self::Exprs, Self::Sources> {
        let mut parts = self.base.into_parts();
        parts.filters.push(self.filter);
        parts
    }
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for OrderBy<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Sources = Base::Sources;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn into_parts(self) -> SelectParts<'conn, Conn, Self::Exprs, Self::Sources> {
        let mut parts = self.base.into_parts();
        parts.orders.push(self.order);
        parts
    }
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for Limited<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Sources = Base::Sources;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn into_parts(self) -> SelectParts<'conn, Conn, Self::Exprs, Self::Sources> {
        let mut parts = self.base.into_parts();
        parts.limit = Some(self.rows);
        parts
    }
}

impl<'conn, 'scope, Conn, Base> SelectAst<'conn, 'scope, Conn> for Offset<Base>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
{
    type Exprs = Base::Exprs;
    type Sources = Base::Sources;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs()
    }

    fn into_parts(self) -> SelectParts<'conn, Conn, Self::Exprs, Self::Sources> {
        let mut parts = self.base.into_parts();
        parts.offset = Some(self.rows);
        parts
    }
}

impl<'conn, 'scope, Conn, Base, Expr, Source> SelectAst<'conn, 'scope, Conn>
    for Join<Base, Expr, Source>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Base::Exprs: PushBack<Expr>,
    Base::Sources: PushBack<Source>,
    <Base::Sources as PushBack<Source>>::Output: SourceSpecList,
    <Base::Exprs as PushBack<Expr>>::Output: Clone + ToTuple,
    Expr: Clone,
    Source: SourceSpec,
{
    type Exprs = <Base::Exprs as PushBack<Expr>>::Output;
    type Sources = <Base::Sources as PushBack<Source>>::Output;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs().push_back(self.expr.clone())
    }

    fn into_parts(self) -> SelectParts<'conn, Conn, Self::Exprs, Self::Sources> {
        let parts = self.base.into_parts();
        SelectParts {
            connection: parts.connection,
            depth: parts.depth,
            exprs: parts.exprs.push_back(self.expr),
            sources: parts.sources.push_back(self.source),
            filters: parts.filters,
            orders: parts.orders,
            limit: parts.limit,
            offset: parts.offset,
        }
    }
}

impl<'conn, 'scope, Conn, Base, Expr, Source> SelectAst<'conn, 'scope, Conn>
    for LeftJoin<Base, Expr, Source>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Base::Exprs: PushBack<Expr>,
    Base::Sources: PushBack<Source>,
    <Base::Sources as PushBack<Source>>::Output: SourceSpecList,
    <Base::Exprs as PushBack<Expr>>::Output: Clone + ToTuple,
    Expr: Clone,
    Source: SourceSpec,
{
    type Exprs = <Base::Exprs as PushBack<Expr>>::Output;
    type Sources = <Base::Sources as PushBack<Source>>::Output;

    fn depth(&self) -> usize {
        self.base.depth()
    }

    fn exprs(&self) -> Self::Exprs {
        self.base.exprs().push_back(self.expr.clone())
    }

    fn into_parts(self) -> SelectParts<'conn, Conn, Self::Exprs, Self::Sources> {
        let parts = self.base.into_parts();
        SelectParts {
            connection: parts.connection,
            depth: parts.depth,
            exprs: parts.exprs.push_back(self.expr),
            sources: parts.sources.push_back(self.source),
            filters: parts.filters,
            orders: parts.orders,
            limit: parts.limit,
            offset: parts.offset,
        }
    }
}

pub trait SourceQuery<'conn, 'scope, Conn>: SelectAst<'conn, 'scope, Conn> + Sized
where
    Conn: QueryBuilder + 'conn,
{
    fn where_(
        self,
        predicate: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> Predicate<'scope>,
    ) -> Where<Self> {
        let predicate = predicate(self.exprs().to_tuple());
        Where {
            base: self,
            filter: Filter::new(predicate.node().clone()),
        }
    }

    fn order_by(
        self,
        order: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> Order<'scope>,
    ) -> OrderBy<Self> {
        let order = order(self.exprs().to_tuple());
        OrderBy {
            base: self,
            order: Sort::new(order.node().clone()),
        }
    }

    fn limit(self, rows: usize) -> Limited<Self> {
        Limited { base: self, rows }
    }

    fn offset(self, rows: usize) -> Offset<Self> {
        Offset { base: self, rows }
    }

    fn join<S>(
        self,
        on: impl FnOnce(
            <Self::Exprs as ToTuple>::Tuple,
            <S as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope>,
    ) -> Join<Self, <S as ProjectionShape>::Exprs<'scope>, InnerJoinSource<S>>
    where
        S: TableProjection,
        <S as ProjectionShape>::Exprs<'scope>: Clone,
    {
        let alias = format!("q{}_{}", self.depth(), Self::Exprs::LEN);
        let right = S::exprs(&alias);
        let join_on = on(self.exprs().to_tuple(), right.clone());
        Join {
            base: self,
            expr: right,
            source: InnerJoinSource::new(alias, join_on.node().clone()),
        }
    }

    fn left_join<S>(
        self,
        on: impl FnOnce(
            <Self::Exprs as ToTuple>::Tuple,
            <S as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope>,
    ) -> LeftJoin<Self, <Maybe<S> as ProjectionShape>::Exprs<'scope>, LeftJoinSource<S>>
    where
        S: TableProjection,
        Maybe<S>: ProjectionShape,
    {
        let alias = format!("q{}_{}", self.depth(), Self::Exprs::LEN);
        let joined = S::exprs(&alias);
        let projection = Maybe::<S>::exprs(&alias);
        let join_on = on(self.exprs().to_tuple(), joined);
        LeftJoin {
            base: self,
            expr: projection,
            source: LeftJoinSource::new(alias, join_on.node().clone()),
        }
    }

    fn select<P>(
        self,
        projection: impl FnOnce(<Self::Exprs as ToTuple>::Tuple) -> P,
    ) -> Conn::Select<'conn, <P as ReturningProjection<'scope>>::Shape>
    where
        P: ReturningProjection<'scope> + Projectable,
        <P as Projectable>::Columns: IrList<SelectColumn>,
        <P as ReturningProjection<'scope>>::Shape: ProjectionShape,
        <<P as ReturningProjection<'scope>>::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
    {
        let parts = self.into_parts();
        let projection = projection(parts.exprs.to_tuple());
        <<Conn as QueryBuilder>::Select<
            'conn,
            <P as ReturningProjection<'scope>>::Shape,
        > as SelectQuery<'conn>>::build(
            parts.connection,
            Select::new(projection.project().into_vec(), parts.sources.into_sources())
                .with_filters(parts.filters)
                .with_orders(parts.orders)
                .with_limit(parts.limit)
                .with_offset(parts.offset),
        )
    }
}

impl<'conn, 'scope, Conn, Query> SourceQuery<'conn, 'scope, Conn> for Query
where
    Conn: QueryBuilder + 'conn,
    Query: SelectAst<'conn, 'scope, Conn>,
{
}

/// Marker for mutation builders that still need a filter or explicit all-rows intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationUnfiltered {}

/// Marker for mutation builders that are safe to execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationFiltered {}

/// Scoped select builder. Each `q` call adds a lateral-capable subquery.
pub struct SelectBuilder<'conn, 'scope, Conn: QueryBuilder> {
    depth: usize,
    sources: Vec<Source>,
    filters: Vec<Filter>,
    orders: Vec<Sort>,
    limit: Option<usize>,
    offset: Option<usize>,
    _phantom: PhantomData<(&'conn Conn, &'scope ())>,
}

/// Scoped delete builder for filtering a single table delete.
pub struct DeleteBuilder<
    'conn,
    'scope,
    Conn: QueryBuilder,
    S: TableProjection,
    FilterState = MutationUnfiltered,
> {
    connection: &'conn Conn,
    depth: usize,
    filters: Vec<Filter>,
    _phantom: PhantomData<(&'scope (), S, FilterState)>,
}

impl<'conn, 'scope, Conn> SelectBuilder<'conn, 'scope, Conn>
where
    Conn: QueryBuilder + 'conn,
{
    fn new(depth: usize) -> Self {
        Self {
            depth,
            sources: Vec::new(),
            filters: Vec::new(),
            orders: Vec::new(),
            limit: None,
            offset: None,
            _phantom: PhantomData,
        }
    }

    /// Add a `FROM` table source and return its expression columns in this select scope.
    pub fn from<S>(&mut self) -> <S as ProjectionShape>::Exprs<'scope>
    where
        S: TableProjection,
    {
        let alias = self.next_alias();
        self.sources
            .push(Source::table(alias.clone(), S::qualified_name()));
        S::exprs(&alias)
    }

    /// Add an inner-joined table source and return its expression columns in this query scope.
    pub fn join<S>(
        &mut self,
        on: impl FnOnce(&<S as ProjectionShape>::Exprs<'scope>) -> Predicate<'scope>,
    ) -> <S as ProjectionShape>::Exprs<'scope>
    where
        S: TableProjection,
    {
        let alias = self.next_alias();
        let projection = S::exprs(&alias);
        let predicate = on(&projection);
        self.sources.push(Source::join(
            alias,
            S::qualified_name(),
            predicate.node().clone(),
        ));
        projection
    }

    /// Add a left-joined table source and return its expression columns in this query scope.
    pub fn left_join<S>(
        &mut self,
        on: impl FnOnce(&<S as ProjectionShape>::Exprs<'scope>) -> Predicate<'scope>,
    ) -> <Maybe<S> as ProjectionShape>::Exprs<'scope>
    where
        S: TableProjection,
        Maybe<S>: ProjectionShape,
    {
        let alias = self.next_alias();
        let projection = S::exprs(&alias);
        let predicate = on(&projection);
        self.sources.push(Source::left_join(
            alias.clone(),
            S::qualified_name(),
            predicate.node().clone(),
        ));
        Maybe::<S>::exprs(&alias)
    }

    /// Add a subquery as a lateral source and return its projected expression columns in this query scope.
    pub fn lateral<'query, Qry>(
        &mut self,
        query: &Qry,
    ) -> <Qry::Shape as ProjectionShape>::ReboundExprs<'scope>
    where
        Qry: SelectQuery<'query, Builder = Conn>,
    {
        let alias = self.next_alias();
        self.sources
            .push(Source::lateral(alias.clone(), query.ir().clone()));
        Qry::Shape::rebound_exprs(&alias)
    }

    /// Add a subquery and return its projected expression columns in this query scope.
    pub fn q<'query, Qry>(
        &mut self,
        query: &Qry,
    ) -> <Qry::Shape as ProjectionShape>::ReboundExprs<'scope>
    where
        Qry: SelectQuery<'query, Builder = Conn>,
    {
        self.lateral(query)
    }

    /// Select the supplied projection value, allowing the query shape to be inferred.
    pub fn returning<P>(
        &mut self,
        projection: P,
    ) -> Returning<<P as ReturningProjection<'scope>>::Shape, <P as Projectable>::Columns>
    where
        P: ReturningProjection<'scope> + Projectable,
    {
        Returning::new(projection.project())
    }

    fn next_alias(&self) -> String {
        format!("q{}_{}", self.depth, self.sources.len())
    }

    /// Add a SQL `WHERE` predicate to the query currently being built.
    pub fn where_(&mut self, predicate: Predicate<'scope>) {
        self.filters.push(Filter::new(predicate.node().clone()));
    }

    /// Add an `ORDER BY` expression to the query currently being built.
    pub fn order_by(&mut self, order: Order<'scope>) {
        self.orders.push(Sort::new(order.node().clone()));
    }

    /// Add a SQL `LIMIT` row count to the query currently being built.
    pub fn limit(&mut self, rows: usize) {
        self.limit = Some(rows);
    }

    /// Add a SQL `OFFSET` row count to the query currently being built.
    pub fn offset(&mut self, rows: usize) {
        self.offset = Some(rows);
    }
}

impl<'conn, 'scope, Conn, S, FilterState> DeleteBuilder<'conn, 'scope, Conn, S, FilterState>
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection + 'conn,
{
    pub(crate) fn new(connection: &'conn Conn, depth: usize) -> Self {
        Self {
            connection,
            depth,
            filters: Vec::new(),
            _phantom: PhantomData,
        }
    }

    /// Return expression columns for the table being deleted.
    pub fn table(&self) -> <S as ProjectionShape>::Exprs<'scope> {
        S::exprs(&self.alias())
    }

    fn alias(&self) -> String {
        format!("q{}_0", self.depth)
    }
}

impl<'conn, Conn, S, FilterState> DeleteBuilder<'conn, 'static, Conn, S, FilterState>
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection + 'conn,
{
    /// Add a SQL `WHERE` predicate to the delete currently being built.
    pub fn where_(
        mut self,
        predicate: impl FnOnce(&<S as ProjectionShape>::Exprs<'static>) -> Predicate<'static>,
    ) -> DeleteBuilder<'conn, 'static, Conn, S, MutationFiltered> {
        let table = S::exprs(&self.alias());
        let predicate = predicate(&table);
        self.filters.push(Filter::new(predicate.node().clone()));
        DeleteBuilder {
            connection: self.connection,
            depth: self.depth,
            filters: self.filters,
            _phantom: PhantomData,
        }
    }

    /// Explicitly mark this delete as intentionally affecting every row.
    pub fn all(self) -> DeleteBuilder<'conn, 'static, Conn, S, MutationFiltered> {
        DeleteBuilder {
            connection: self.connection,
            depth: self.depth,
            filters: self.filters,
            _phantom: PhantomData,
        }
    }
}

impl<'conn, Conn, S> DeleteBuilder<'conn, 'static, Conn, S, MutationFiltered>
where
    Conn: Connection + 'conn,
    S: TableProjection + 'conn,
    <Conn as QueryBuilder>::Delete<'conn, S, ()>: ExecutableDeleteQuery<'conn>,
{
    pub fn execute(self) -> impl Future<Output = Result<u64, ErrorOf<Conn>>> + 'conn {
        let query = <<Conn as QueryBuilder>::Delete<'conn, S, ()> as DeleteQuery<'conn>>::build(
            self.connection,
            build_delete::<S>(self.alias(), self.filters),
        );
        async move { ExecutableDeleteQuery::execute(&query).await }
    }
}

impl<'conn, Conn, S> DeleteBuilder<'conn, 'static, Conn, S, MutationFiltered>
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection + 'conn,
{
    pub fn returning<P>(
        self,
        projection: impl FnOnce(<S as ProjectionShape>::Exprs<'static>) -> P,
    ) -> Conn::Delete<'conn, S, <P as ReturningProjection<'static>>::Shape>
    where
        P: ReturningProjection<'static> + Projectable,
        <P::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
        <P as Projectable>::Columns: IrList<SelectColumn>,
    {
        let table = S::exprs(&self.alias());
        let projection = projection(table);
        <<Conn as QueryBuilder>::Delete<
            'conn,
            S,
            <P as ReturningProjection<'static>>::Shape,
        > as DeleteQuery<'conn>>::build(
            self.connection,
            build_delete_returning::<S>(self.alias(), self.filters, projection.project()),
        )
    }
}

thread_local! {
    static QUERY_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Build select IR from a scoped query builder closure returning a shape-carrying projection.
///
/// Expressions created by the builder are scoped to that builder invocation and cannot be
/// smuggled out as reusable values:
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
/// let mut leaked = None;
/// let _ = conn.select(|q| {
///     let user = q.from::<User>();
///     leaked = Some(user.clone());
///     q.returning(user)
/// });
/// let _ = leaked.unwrap();
/// ```
pub fn build_select<'conn, Conn, Shape, Columns>(
    f: impl for<'scope> FnOnce(&mut SelectBuilder<'conn, 'scope, Conn>) -> Returning<Shape, Columns>,
) -> Select
where
    Conn: QueryBuilder + 'conn,
    Shape: ProjectionShape,
    Columns: IrList<SelectColumn>,
{
    QUERY_DEPTH.with(|depth| {
        let current_depth = depth.get();
        depth.set(current_depth + 1);

        let mut q = SelectBuilder::new(current_depth);
        let output = f(&mut q);

        depth.set(current_depth);

        Select::new(output.into_columns().into_vec(), q.sources)
            .with_filters(q.filters)
            .with_orders(q.orders)
            .with_limit(q.limit)
            .with_offset(q.offset)
    })
}

/// Construct the initial consuming source-first select builder.
pub fn build_from_builder<'conn, Conn, S>(
    connection: &'conn Conn,
) -> From<
    'conn,
    'conn,
    Conn,
    HCons<<S as ProjectionShape>::Exprs<'conn>, HNil>,
    HCons<RootSource<S>, HNil>,
>
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection,
{
    QUERY_DEPTH.with(|depth| {
        let current_depth = depth.get();
        depth.set(current_depth + 1);
        let builder = From::new(connection, current_depth);
        depth.set(current_depth);
        builder
    })
}

/// Build insert IR for a table and ordered column bindings.
pub fn build_insert<S>(columns: impl IrList<InsertColumn>) -> Insert
where
    S: InsertableTable,
{
    build_insert_returning::<S>(columns, ())
}

/// Build insert IR for a table, ordered column bindings, and returned columns.
pub fn build_insert_returning<S>(
    columns: impl IrList<InsertColumn>,
    returning: impl IrList<SelectColumn>,
) -> Insert
where
    S: InsertableTable,
{
    Insert::new(
        <S as SchemaTable>::qualified_name(),
        columns.into_vec(),
        returning.into_vec(),
    )
}

/// Build update IR for a table, ordered column bindings, and filters.
pub fn build_update<S>(
    alias: impl Into<String>,
    columns: impl IrList<UpdateColumn>,
    filters: Vec<Filter>,
) -> Update
where
    S: UpdateableTable,
{
    build_update_returning::<S>(alias, columns, filters, ())
}

/// Build update IR for a table, ordered column bindings, filters, and returned columns.
pub fn build_update_returning<S>(
    alias: impl Into<String>,
    columns: impl IrList<UpdateColumn>,
    filters: Vec<Filter>,
    returning: impl IrList<SelectColumn>,
) -> Update
where
    S: UpdateableTable,
{
    Update::new(
        <S as SchemaTable>::qualified_name(),
        alias,
        columns.into_vec(),
        filters,
        returning.into_vec(),
    )
}

/// Build delete IR for a table, SQL alias, and filters.
pub fn build_delete<S>(alias: impl Into<String>, filters: Vec<Filter>) -> Delete
where
    S: TableProjection,
{
    build_delete_returning::<S>(alias, filters, ())
}

/// Build delete IR for a table, SQL alias, filters, and returned columns.
pub fn build_delete_returning<S>(
    alias: impl Into<String>,
    filters: Vec<Filter>,
    returning: impl IrList<SelectColumn>,
) -> Delete
where
    S: TableProjection,
{
    Delete::new(S::qualified_name(), alias, returning.into_vec()).with_filters(filters)
}

/// Construct the initial delete builder.
pub fn build_delete_builder<'conn, Conn, S>(
    connection: &'conn Conn,
) -> DeleteBuilder<'conn, 'static, Conn, S>
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection + 'conn,
{
    QUERY_DEPTH.with(|depth| {
        let current_depth = depth.get();
        depth.set(current_depth + 1);
        let builder = DeleteBuilder::new(connection, current_depth);
        depth.set(current_depth);
        builder
    })
}
