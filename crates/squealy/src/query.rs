use std::cell::Cell;
use std::marker::PhantomData;

use crate::{
    Connection, Order, OrderNode, Predicate, PredicateNode, Projectable, ProjectionShape,
    SelectColumn, TableProjection,
};

/// A backend-specific query object backed by core-owned select IR.
pub trait Query {
    type Connection: Connection;
    type Shape: ProjectionShape;

    fn ir(&self) -> &Select;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    columns: Vec<SelectColumn>,
    sources: Vec<Source>,
    filters: Vec<Filter>,
    orders: Vec<Sort>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl Select {
    fn new(columns: Vec<SelectColumn>, sources: Vec<Source>) -> Self {
        Self {
            columns,
            sources,
            filters: Vec::new(),
            orders: Vec::new(),
            limit: None,
            offset: None,
        }
    }

    fn with_filters(mut self, filters: Vec<Filter>) -> Self {
        self.filters = filters;
        self
    }

    fn with_orders(mut self, orders: Vec<Sort>) -> Self {
        self.orders = orders;
        self
    }

    fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    fn with_offset(mut self, offset: Option<usize>) -> Self {
        self.offset = offset;
        self
    }

    pub fn columns(&self) -> &[SelectColumn] {
        &self.columns
    }

    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    pub fn orders(&self) -> &[Sort] {
        &self.orders
    }

    pub fn limit(&self) -> Option<usize> {
        self.limit
    }

    pub fn offset(&self) -> Option<usize> {
        self.offset
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    alias: String,
    kind: SourceKind,
    target: SourceTarget,
}

impl Source {
    fn table(alias: impl Into<String>, table: impl ToString) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::From,
            target: SourceTarget::Table(table.to_string()),
        }
    }

    fn lateral(alias: impl Into<String>, query: Select) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::InnerLateral,
            target: SourceTarget::Query(Box::new(query)),
        }
    }

    fn join(alias: impl Into<String>, table: impl ToString, on: Predicate<'_>) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::InnerJoin {
                on: on.node().clone(),
            },
            target: SourceTarget::Table(table.to_string()),
        }
    }

    fn left_join(alias: impl Into<String>, table: impl ToString, on: Predicate<'_>) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::LeftJoin {
                on: on.node().clone(),
            },
            target: SourceTarget::Table(table.to_string()),
        }
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn kind(&self) -> &SourceKind {
        &self.kind
    }

    pub fn target(&self) -> &SourceTarget {
        &self.target
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceKind {
    From,
    InnerLateral,
    InnerJoin { on: PredicateNode },
    LeftJoin { on: PredicateNode },
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceTarget {
    Table(String),
    Query(Box<Select>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    predicate: PredicateNode,
}

impl Filter {
    fn new(predicate: Predicate<'_>) -> Self {
        Self {
            predicate: predicate.node().clone(),
        }
    }

    pub fn predicate(&self) -> &PredicateNode {
        &self.predicate
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sort {
    order: OrderNode,
}

impl Sort {
    fn new(order: Order<'_>) -> Self {
        Self {
            order: order.node().clone(),
        }
    }

    pub fn order(&self) -> &OrderNode {
        &self.order
    }
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
        self.sources
            .push(Source::join(alias, S::qualified_name(), predicate));
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
        self.sources
            .push(Source::left_join(alias, S::qualified_name(), predicate));
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
        self.filters.push(Filter::new(predicate));
    }

    /// Add an `ORDER BY` expression to the query currently being built.
    pub fn order_by(&mut self, order: Order<'scope>) {
        self.orders.push(Sort::new(order));
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
