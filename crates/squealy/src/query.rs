use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::marker::PhantomData;

use futures_core::Stream;

use crate::ir::{
    Delete, Filter, Insert, InsertColumn, PredicateNode, Select, Sort, Source, Update, UpdateColumn,
};
use crate::{
    Backend, ColumnRef, Connection, Decode, Expr, ExprKind, InsertableTable, IntoBindValue, IrList,
    Maybe, Order, Predicate, Projectable, ProjectionShape, QueryBuilder, SchemaTable, SelectColumn,
    TableProjection, TupleAppend, TupleLen, TuplePush, UpdateableTable,
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

#[doc(hidden)]
pub trait SourceSpecList {
    fn into_sources(self) -> Vec<Source>;
}

impl SourceSpecList for () {
    fn into_sources(self) -> Vec<Source> {
        Vec::new()
    }
}

squealy_macros::tuple_source_spec_lists!(32);

struct SourceChain<'conn, 'scope, const N: usize, Conn, Exprs, Sources, Filters>
where
    Conn: QueryBuilder,
    Exprs: TupleLen<N>,
    Sources: SourceSpecList + TupleLen<N>,
    Filters: IrList<Filter>,
{
    connection: &'conn Conn,
    depth: usize,
    exprs: Exprs,
    sources: Sources,
    filters: Filters,
    _scope: PhantomData<&'scope ()>,
}

/// A consuming, source-first select builder carrying `N` typed sources.
pub struct From<'conn, 'scope, Conn, const N: usize, Exprs, Sources, Filters = ()>
where
    Conn: QueryBuilder,
    Exprs: TupleLen<N>,
    Sources: SourceSpecList + TupleLen<N>,
    Filters: IrList<Filter>,
{
    chain: SourceChain<'conn, 'scope, N, Conn, Exprs, Sources, Filters>,
}

impl<'conn, 'scope, Conn, S>
    From<'conn, 'scope, Conn, 1, (<S as ProjectionShape>::Exprs<'scope>,), (RootSource<S>,)>
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection,
{
    pub(crate) fn new(connection: &'conn Conn, depth: usize) -> Self {
        let alias = format!("q{depth}_0");
        Self {
            chain: SourceChain {
                connection,
                depth,
                exprs: (S::exprs(&alias),),
                sources: (RootSource::new(alias),),
                filters: (),
                _scope: PhantomData,
            },
        }
    }
}

impl<'conn, 'scope, Conn, S, Filters>
    From<
        'conn,
        'scope,
        Conn,
        1,
        (<S as ProjectionShape>::Exprs<'scope>,),
        (RootSource<S>,),
        Filters,
    >
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection,
    Filters: IrList<Filter>,
    <S as ProjectionShape>::Exprs<'scope>: Clone,
{
    pub fn where_(
        self,
        predicate: impl FnOnce(&<S as ProjectionShape>::Exprs<'scope>) -> Predicate<'scope>,
    ) -> From<
        'conn,
        'scope,
        Conn,
        1,
        (<S as ProjectionShape>::Exprs<'scope>,),
        (RootSource<S>,),
        <Filters as TupleAppend<Filter>>::Output,
    >
    where
        Filters: TupleAppend<Filter>,
    {
        let predicate = predicate(&self.chain.exprs.0);
        From {
            chain: SourceChain {
                connection: self.chain.connection,
                depth: self.chain.depth,
                exprs: self.chain.exprs,
                sources: self.chain.sources,
                filters: self
                    .chain
                    .filters
                    .append(Filter::new(predicate.node().clone())),
                _scope: PhantomData,
            },
        }
    }

    pub fn join<J>(
        self,
        on: impl FnOnce(
            &<S as ProjectionShape>::Exprs<'scope>,
            &<J as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope>,
    ) -> From<
        'conn,
        'scope,
        Conn,
        2,
        (
            <S as ProjectionShape>::Exprs<'scope>,
            <J as ProjectionShape>::Exprs<'scope>,
        ),
        (RootSource<S>, InnerJoinSource<J>),
        Filters,
    >
    where
        J: TableProjection,
    {
        let alias = format!("q{}_1", self.chain.depth);
        let right = J::exprs(&alias);
        let join_on = on(&self.chain.exprs.0, &right);
        From {
            chain: SourceChain {
                connection: self.chain.connection,
                depth: self.chain.depth,
                exprs: self.chain.exprs.push(right),
                sources: self
                    .chain
                    .sources
                    .push(InnerJoinSource::new(alias, join_on.node().clone())),
                filters: self.chain.filters,
                _scope: PhantomData,
            },
        }
    }

    pub fn select<P>(
        self,
        projection: impl FnOnce(<S as ProjectionShape>::Exprs<'scope>) -> P,
    ) -> Conn::Select<'conn, <P as ReturningProjection<'scope>>::Shape>
    where
        P: ReturningProjection<'scope> + Projectable,
        <P as Projectable>::Columns: IrList<SelectColumn>,
        <P as ReturningProjection<'scope>>::Shape: ProjectionShape,
        <<P as ReturningProjection<'scope>>::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
    {
        let projection = projection(self.chain.exprs.0.clone());
        <<Conn as QueryBuilder>::Select<
            'conn,
            <P as ReturningProjection<'scope>>::Shape,
        > as SelectQuery<'conn>>::build(
            self.chain.connection,
            Select::new(
                projection.project().into_vec(),
                self.chain.sources.into_sources(),
            )
            .with_filters(self.chain.filters.into_vec()),
        )
    }
}

impl<'conn, 'scope, Conn, S, J, Filters>
    From<
        'conn,
        'scope,
        Conn,
        2,
        (
            <S as ProjectionShape>::Exprs<'scope>,
            <J as ProjectionShape>::Exprs<'scope>,
        ),
        (RootSource<S>, InnerJoinSource<J>),
        Filters,
    >
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection,
    J: TableProjection,
    Filters: IrList<Filter>,
    <S as ProjectionShape>::Exprs<'scope>: Clone,
    <J as ProjectionShape>::Exprs<'scope>: Clone,
{
    pub fn where_(
        self,
        predicate: impl FnOnce(
            &<S as ProjectionShape>::Exprs<'scope>,
            &<J as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope>,
    ) -> From<
        'conn,
        'scope,
        Conn,
        2,
        (
            <S as ProjectionShape>::Exprs<'scope>,
            <J as ProjectionShape>::Exprs<'scope>,
        ),
        (RootSource<S>, InnerJoinSource<J>),
        <Filters as TupleAppend<Filter>>::Output,
    >
    where
        Filters: TupleAppend<Filter>,
    {
        let predicate = predicate(&self.chain.exprs.0, &self.chain.exprs.1);
        From {
            chain: SourceChain {
                connection: self.chain.connection,
                depth: self.chain.depth,
                exprs: self.chain.exprs,
                sources: self.chain.sources,
                filters: self
                    .chain
                    .filters
                    .append(Filter::new(predicate.node().clone())),
                _scope: PhantomData,
            },
        }
    }

    pub fn join<K>(
        self,
        on: impl FnOnce(
            &<S as ProjectionShape>::Exprs<'scope>,
            &<J as ProjectionShape>::Exprs<'scope>,
            &<K as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope>,
    ) -> From<
        'conn,
        'scope,
        Conn,
        3,
        (
            <S as ProjectionShape>::Exprs<'scope>,
            <J as ProjectionShape>::Exprs<'scope>,
            <K as ProjectionShape>::Exprs<'scope>,
        ),
        (RootSource<S>, InnerJoinSource<J>, InnerJoinSource<K>),
        Filters,
    >
    where
        K: TableProjection,
    {
        let alias = format!("q{}_2", self.chain.depth);
        let right = K::exprs(&alias);
        let join_on = on(&self.chain.exprs.0, &self.chain.exprs.1, &right);
        From {
            chain: SourceChain {
                connection: self.chain.connection,
                depth: self.chain.depth,
                exprs: self.chain.exprs.push(right),
                sources: self
                    .chain
                    .sources
                    .push(InnerJoinSource::new(alias, join_on.node().clone())),
                filters: self.chain.filters,
                _scope: PhantomData,
            },
        }
    }

    pub fn select<P>(
        self,
        projection: impl FnOnce(
            <S as ProjectionShape>::Exprs<'scope>,
            <J as ProjectionShape>::Exprs<'scope>,
        ) -> P,
    ) -> Conn::Select<'conn, <P as ReturningProjection<'scope>>::Shape>
    where
        P: ReturningProjection<'scope> + Projectable,
        <P as Projectable>::Columns: IrList<SelectColumn>,
        <P as ReturningProjection<'scope>>::Shape: ProjectionShape,
        <<P as ReturningProjection<'scope>>::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
    {
        let projection = projection(self.chain.exprs.0.clone(), self.chain.exprs.1.clone());
        <<Conn as QueryBuilder>::Select<
            'conn,
            <P as ReturningProjection<'scope>>::Shape,
        > as SelectQuery<'conn>>::build(
            self.chain.connection,
            Select::new(
                projection.project().into_vec(),
                self.chain.sources.into_sources(),
            )
            .with_filters(self.chain.filters.into_vec()),
        )
    }
}

impl<'conn, 'scope, Conn, S, J0, J1, Filters>
    From<
        'conn,
        'scope,
        Conn,
        3,
        (
            <S as ProjectionShape>::Exprs<'scope>,
            <J0 as ProjectionShape>::Exprs<'scope>,
            <J1 as ProjectionShape>::Exprs<'scope>,
        ),
        (RootSource<S>, InnerJoinSource<J0>, InnerJoinSource<J1>),
        Filters,
    >
where
    Conn: QueryBuilder + 'conn,
    S: TableProjection,
    J0: TableProjection,
    J1: TableProjection,
    Filters: IrList<Filter>,
    <S as ProjectionShape>::Exprs<'scope>: Clone,
    <J0 as ProjectionShape>::Exprs<'scope>: Clone,
    <J1 as ProjectionShape>::Exprs<'scope>: Clone,
{
    pub fn where_(
        self,
        predicate: impl FnOnce(
            &<S as ProjectionShape>::Exprs<'scope>,
            &<J0 as ProjectionShape>::Exprs<'scope>,
            &<J1 as ProjectionShape>::Exprs<'scope>,
        ) -> Predicate<'scope>,
    ) -> From<
        'conn,
        'scope,
        Conn,
        3,
        (
            <S as ProjectionShape>::Exprs<'scope>,
            <J0 as ProjectionShape>::Exprs<'scope>,
            <J1 as ProjectionShape>::Exprs<'scope>,
        ),
        (RootSource<S>, InnerJoinSource<J0>, InnerJoinSource<J1>),
        <Filters as TupleAppend<Filter>>::Output,
    >
    where
        Filters: TupleAppend<Filter>,
    {
        let predicate = predicate(
            &self.chain.exprs.0,
            &self.chain.exprs.1,
            &self.chain.exprs.2,
        );
        From {
            chain: SourceChain {
                connection: self.chain.connection,
                depth: self.chain.depth,
                exprs: self.chain.exprs,
                sources: self.chain.sources,
                filters: self
                    .chain
                    .filters
                    .append(Filter::new(predicate.node().clone())),
                _scope: PhantomData,
            },
        }
    }

    pub fn select<P>(
        self,
        projection: impl FnOnce(
            <S as ProjectionShape>::Exprs<'scope>,
            <J0 as ProjectionShape>::Exprs<'scope>,
            <J1 as ProjectionShape>::Exprs<'scope>,
        ) -> P,
    ) -> Conn::Select<'conn, <P as ReturningProjection<'scope>>::Shape>
    where
        P: ReturningProjection<'scope> + Projectable,
        <P as Projectable>::Columns: IrList<SelectColumn>,
        <P as ReturningProjection<'scope>>::Shape: ProjectionShape,
        <<P as ReturningProjection<'scope>>::Shape as ProjectionShape>::Row: Decode<Conn::Backend>,
    {
        let projection = projection(
            self.chain.exprs.0.clone(),
            self.chain.exprs.1.clone(),
            self.chain.exprs.2.clone(),
        );
        <<Conn as QueryBuilder>::Select<
            'conn,
            <P as ReturningProjection<'scope>>::Shape,
        > as SelectQuery<'conn>>::build(
            self.chain.connection,
            Select::new(
                projection.project().into_vec(),
                self.chain.sources.into_sources(),
            )
            .with_filters(self.chain.filters.into_vec()),
        )
    }
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
) -> From<'conn, 'conn, Conn, 1, (<S as ProjectionShape>::Exprs<'conn>,), (RootSource<S>,), ()>
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
