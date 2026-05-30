use std::cell::Cell;
use std::future::Future;
use std::marker::PhantomData;

use futures_core::Stream;

use crate::ir::{Delete, Filter, Insert, InsertColumn, Select, Sort, Source, Update, UpdateColumn};
use crate::{
    ColumnRef, Connection, Expr, ExprKind, InsertableTable, IntoBindValue, Maybe, Order, Predicate,
    Projectable, ProjectionShape, SchemaTable, SelectColumn, TableProjection, UpdateableTable,
};

/// A backend-specific select query object backed by core-owned select IR.
pub trait SelectQuery<'conn> {
    type Connection: Connection + 'conn;
    type Shape: ProjectionShape;
    type Row: Send;

    type RowStream<'query>: Stream<Item = Result<Self::Row, <Self::Connection as Connection>::Error>>
        + Send
        + 'query
    where
        Self: 'query;

    fn ir(&self) -> &Select;

    fn fetch(&self) -> Self::RowStream<'_>;

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_;
}

/// A backend-specific insert query object backed by core-owned insert IR.
pub trait InsertQuery<'conn> {
    type Connection: Connection + 'conn;
    type Table: InsertableTable;
    type Shape: ProjectionShape;
    type Row: Send;

    type RowStream<'query>: Stream<Item = Result<Self::Row, <Self::Connection as Connection>::Error>>
        + Send
        + 'query
    where
        Self: 'query;

    fn ir(&self) -> &Insert;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch(&self) -> Self::RowStream<'_>;

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_;
}

/// A backend-specific update query object backed by core-owned update IR.
pub trait UpdateQuery<'conn> {
    type Connection: Connection + 'conn;
    type Table: UpdateableTable;
    type Shape: ProjectionShape;
    type Row: Send;

    type RowStream<'query>: Stream<Item = Result<Self::Row, <Self::Connection as Connection>::Error>>
        + Send
        + 'query
    where
        Self: 'query;

    fn ir(&self) -> &Update;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch(&self) -> Self::RowStream<'_>;

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_;
}

/// A backend-specific delete query object backed by core-owned delete IR.
pub trait DeleteQuery<'conn> {
    type Connection: Connection + 'conn;
    type Table: TableProjection;
    type Shape: ProjectionShape;
    type Row: Send;

    type RowStream<'query>: Stream<Item = Result<Self::Row, <Self::Connection as Connection>::Error>>
        + Send
        + 'query
    where
        Self: 'query;

    fn ir(&self) -> &Delete;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch(&self) -> Self::RowStream<'_>;

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_;

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_;
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
pub struct Returning<Shape>
where
    Shape: ProjectionShape,
{
    columns: Vec<SelectColumn>,
    _shape: PhantomData<Shape>,
}

impl<Shape> Returning<Shape>
where
    Shape: ProjectionShape,
{
    fn new(columns: Vec<SelectColumn>) -> Self {
        Self {
            columns,
            _shape: PhantomData,
        }
    }

    fn into_columns(self) -> Vec<SelectColumn> {
        self.columns
    }
}

/// Marker for mutation builders that still need a filter or explicit all-rows intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationUnfiltered {}

/// Marker for mutation builders that are safe to execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationFiltered {}

/// Scoped select builder. Each `q` call adds a lateral-capable subquery.
pub struct SelectBuilder<'conn, 'scope, Conn: Connection> {
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
    Conn: Connection,
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
    Conn: Connection + 'conn,
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
        Qry: SelectQuery<'query, Connection = Conn>,
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
        Qry: SelectQuery<'query, Connection = Conn>,
    {
        self.lateral(query)
    }

    /// Select the supplied projection value, allowing the query shape to be inferred.
    pub fn returning<P>(
        &mut self,
        projection: P,
    ) -> Returning<<P as ReturningProjection<'scope>>::Shape>
    where
        P: ReturningProjection<'scope>,
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
    Conn: Connection + 'conn,
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
    Conn: Connection + 'conn,
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
{
    pub fn execute(self) -> impl Future<Output = Result<u64, <Conn as Connection>::Error>> + 'conn {
        let query = Connection::delete_query::<S>(self.connection, self.alias(), self.filters);
        async move { DeleteQuery::execute(&query).await }
    }

    pub fn returning<P>(
        self,
        projection: impl FnOnce(<S as ProjectionShape>::Exprs<'static>) -> P,
    ) -> Conn::Delete<'conn, S, <P as ReturningProjection<'static>>::Shape>
    where
        P: ReturningProjection<'static>,
    {
        let table = S::exprs(&self.alias());
        let projection = projection(table);
        Connection::delete_returning_query::<S, <P as ReturningProjection<'static>>::Shape>(
            self.connection,
            self.alias(),
            self.filters,
            projection.project(),
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
/// # use std::marker::PhantomData;
///
/// #[derive(Clone, Table)]
/// struct User<'scope, C: ColumnMode = ColumnExpr> {
///     id: C::Type<'scope, i32>,
/// }
/// #
/// # struct DocConnection;
/// #
/// # struct DocSelect<'conn, Shape> {
/// #     select: Select,
/// #     _connection: PhantomData<&'conn DocConnection>,
/// #     _shape: PhantomData<Shape>,
/// # }
/// #
/// # impl<'conn, Shape> SelectQuery<'conn> for DocSelect<'conn, Shape>
/// # where
/// #     Shape: ProjectionShape,
/// # {
/// #     type Connection = DocConnection;
/// #     type Shape = Shape;
/// #
/// #     fn ir(&self) -> &Select {
/// #         &self.select
/// #     }
/// # }
/// #
/// # impl Connection for DocConnection {
/// #     type Error = ();
/// #
/// #     type Select<'conn, Shape> = DocSelect<'conn, Shape>
/// #     where
/// #         Self: 'conn,
/// #         Shape: ProjectionShape;
/// #
/// #     fn select<Shape>(
/// #         &self,
/// #         f: impl for<'scope> FnOnce(
/// #             &mut ::squealy::SelectBuilder<'_, 'scope, Self>,
/// #         ) -> Returning<Shape>,
/// #     ) -> Self::Select<'_, Shape>
/// #     where
/// #         Shape: ProjectionShape,
/// #     {
/// #         DocSelect {
/// #             select: build_select::<Self, Shape>(f),
/// #             _connection: PhantomData,
/// #             _shape: PhantomData,
/// #         }
/// #     }
/// # }
///
/// let conn = DocConnection;
/// let mut leaked = None;
/// let _ = conn.select(|q| {
///     let user = q.from::<User>();
///     leaked = Some(user.clone());
///     q.returning(user)
/// });
/// let _ = leaked.unwrap();
/// ```
pub fn build_select<'conn, Conn, Shape>(
    f: impl for<'scope> FnOnce(&mut SelectBuilder<'conn, 'scope, Conn>) -> Returning<Shape>,
) -> Select
where
    Conn: Connection + 'conn,
    Shape: ProjectionShape,
{
    QUERY_DEPTH.with(|depth| {
        let current_depth = depth.get();
        depth.set(current_depth + 1);

        let mut q = SelectBuilder::new(current_depth);
        let output = f(&mut q);

        depth.set(current_depth);

        Select::new(output.into_columns(), q.sources)
            .with_filters(q.filters)
            .with_orders(q.orders)
            .with_limit(q.limit)
            .with_offset(q.offset)
    })
}

/// Build insert IR for a table and ordered column bindings.
pub fn build_insert<S>(columns: Vec<InsertColumn>) -> Insert
where
    S: InsertableTable,
{
    build_insert_returning::<S>(columns, Vec::new())
}

/// Build insert IR for a table, ordered column bindings, and returned columns.
pub fn build_insert_returning<S>(columns: Vec<InsertColumn>, returning: Vec<SelectColumn>) -> Insert
where
    S: InsertableTable,
{
    Insert::new(<S as SchemaTable>::qualified_name(), columns, returning)
}

/// Build update IR for a table, ordered column bindings, and filters.
pub fn build_update<S>(
    alias: impl Into<String>,
    columns: Vec<UpdateColumn>,
    filters: Vec<Filter>,
) -> Update
where
    S: UpdateableTable,
{
    build_update_returning::<S>(alias, columns, filters, Vec::new())
}

/// Build update IR for a table, ordered column bindings, filters, and returned columns.
pub fn build_update_returning<S>(
    alias: impl Into<String>,
    columns: Vec<UpdateColumn>,
    filters: Vec<Filter>,
    returning: Vec<SelectColumn>,
) -> Update
where
    S: UpdateableTable,
{
    Update::new(
        <S as SchemaTable>::qualified_name(),
        alias,
        columns,
        filters,
        returning,
    )
}

/// Build delete IR for a table, SQL alias, and filters.
pub fn build_delete<S>(alias: impl Into<String>, filters: Vec<Filter>) -> Delete
where
    S: TableProjection,
{
    build_delete_returning::<S>(alias, filters, Vec::new())
}

/// Build delete IR for a table, SQL alias, filters, and returned columns.
pub fn build_delete_returning<S>(
    alias: impl Into<String>,
    filters: Vec<Filter>,
    returning: Vec<SelectColumn>,
) -> Delete
where
    S: TableProjection,
{
    Delete::new(S::qualified_name(), alias, returning).with_filters(filters)
}

/// Construct the initial delete builder.
pub fn build_delete_builder<'conn, Conn, S>(
    connection: &'conn Conn,
) -> DeleteBuilder<'conn, 'static, Conn, S>
where
    Conn: Connection + 'conn,
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
