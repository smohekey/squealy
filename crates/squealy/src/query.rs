use std::cell::Cell;
use std::marker::PhantomData;

use crate::ir::{Filter, Select, Sort, Source};
use crate::{Connection, Order, Predicate, Projectable, ProjectionShape, TableProjection};

/// A backend-specific query object backed by core-owned select IR.
pub trait Query {
    type Connection: Connection;
    type Shape: ProjectionShape;

    fn ir(&self) -> &Select;
}

/// Scoped query builder. Each `q` call adds a lateral-capable subquery.
pub struct Q<'scope, Conn: Connection> {
    depth: usize,
    sources: Vec<Source>,
    filters: Vec<Filter>,
    orders: Vec<Sort>,
    limit: Option<usize>,
    offset: Option<usize>,
    _phantom: PhantomData<(&'scope (), Conn)>,
}

impl<'scope, Conn> Q<'scope, Conn>
where
    Conn: Connection,
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

    /// Add a table source and return its expression columns in this query scope.
    pub fn each<S>(&mut self) -> <S as ProjectionShape>::Exprs<'scope>
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
    ) -> <S as ProjectionShape>::Exprs<'scope>
    where
        S: TableProjection,
    {
        let alias = self.next_alias();
        let projection = S::exprs(&alias);
        let predicate = on(&projection);
        self.sources.push(Source::left_join(
            alias,
            S::qualified_name(),
            predicate.node().clone(),
        ));
        projection
    }

    /// Add a subquery as a lateral source and return its projected expression columns in this query scope.
    pub fn lateral<Qry>(&mut self, query: &Qry) -> <Qry::Shape as ProjectionShape>::Exprs<'scope>
    where
        Qry: Query<Connection = Conn>,
    {
        let alias = self.next_alias();
        self.sources
            .push(Source::lateral(alias.clone(), query.ir().clone()));
        Qry::Shape::exprs(&alias)
    }

    /// Add a subquery and return its projected expression columns in this query scope.
    pub fn q<Qry>(&mut self, query: &Qry) -> <Qry::Shape as ProjectionShape>::Exprs<'scope>
    where
        Qry: Query<Connection = Conn>,
    {
        self.lateral(query)
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

/// Build select IR from a scoped query builder closure.
///
/// Expressions created by the builder are scoped to that builder invocation and cannot be
/// smuggled out as reusable values:
///
/// ```compile_fail
/// use squealy::*;
/// # use std::{io, marker::PhantomData};
///
/// #[derive(Clone, Table)]
/// struct User<'scope, C: ColumnMode = ColumnExpr> {
///     id: C::Type<'scope, i32>,
/// }
/// #
/// # struct DocConnection;
/// #
/// # struct DocQuery<Shape> {
/// #     select: Select,
/// #     _shape: PhantomData<Shape>,
/// # }
/// #
/// # impl<Shape> Query for DocQuery<Shape>
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
/// # impl Backend for DocConnection {
/// #     fn write_query<Qry>(&self, _query: &Qry, _writer: &mut impl io::Write) -> io::Result<()>
/// #     where
/// #         Self: Connection,
/// #         Qry: Query<Connection = Self>,
/// #     {
/// #         Ok(())
/// #     }
/// #
/// #     fn write_table(
/// #         &self,
/// #         _table: &(dyn Table + Sync),
/// #         _writer: &mut impl io::Write,
/// #     ) -> io::Result<()> {
/// #         Ok(())
/// #     }
/// # }
/// #
/// # impl Connection for DocConnection {
/// #     type Query<Shape> = DocQuery<Shape>
/// #     where
/// #         Shape: ProjectionShape;
/// #
/// #     fn query<Shape>(
/// #         &self,
/// #         f: impl for<'scope> FnOnce(
/// #             &mut ::squealy::Q<'scope, Self>,
/// #         ) -> <Shape as ProjectionShape>::Exprs<'scope>,
/// #     ) -> Self::Query<Shape>
/// #     where
/// #         Shape: ProjectionShape,
/// #     {
/// #         DocQuery {
/// #             select: build_select::<Self, Shape>(f),
/// #             _shape: PhantomData,
/// #         }
/// #     }
/// # }
///
/// let conn = DocConnection;
/// let mut leaked = None;
/// let _ = conn.query::<User>(|q| {
///     let user = q.each::<User>();
///     leaked = Some(user.clone());
///     user
/// });
/// let _ = leaked.unwrap();
/// ```
pub fn build_select<Conn, Shape>(
    f: impl for<'scope> FnOnce(&mut Q<'scope, Conn>) -> <Shape as ProjectionShape>::Exprs<'scope>,
) -> Select
where
    Conn: Connection,
    Shape: ProjectionShape,
{
    QUERY_DEPTH.with(|depth| {
        let current_depth = depth.get();
        depth.set(current_depth + 1);

        let mut q = Q::new(current_depth);
        let output = f(&mut q);

        depth.set(current_depth);

        let select = Select::new(output.project(), q.sources)
            .with_filters(q.filters)
            .with_orders(q.orders)
            .with_limit(q.limit)
            .with_offset(q.offset);

        select
    })
}
