use std::cell::Cell;
use std::future::Future;
use std::marker::PhantomData;

use futures_core::Stream;

use crate::ir::{Filter, Select, Sort, Source};
use crate::{
    ColumnRef, Connection, Expr, ExprKind, IntoBindValue, Maybe, Order, Predicate, Projectable,
    ProjectionShape, SelectColumn, TableProjection,
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
