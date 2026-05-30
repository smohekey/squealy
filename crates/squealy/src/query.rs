use std::cell::Cell;
use std::io::{self, Write};
use std::marker::PhantomData;

use crate::{Order, Predicate, Projectable, ProjectionShape, SelectColumn, TableProjection};

/// A SQL select statement that produces rows with projection shape `Shape`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query<Shape = ()> {
    select: Select,
    sql: String,
    _shape: PhantomData<Shape>,
}

impl<Shape> Query<Shape> {
    fn new(select: Select) -> Self {
        let sql = select.to_sql();

        Self {
            select,
            sql,
            _shape: PhantomData,
        }
    }

    /// Render this query to SQL.
    pub fn to_sql(&self) -> &str {
        &self.sql
    }

    /// Write this query's SQL to a writer.
    pub fn write_sql(&self, writer: &mut impl Write) -> io::Result<()> {
        self.select.write_sql(writer)
    }
}

impl<Shape> Query<Shape>
where
    Shape: ProjectionShape,
{
    /// Build expressions that reference this query's output columns through a SQL alias.
    pub fn project<'scope>(&self, alias: &str) -> Shape::Exprs<'scope> {
        Shape::exprs(alias)
    }
}

impl Query<()> {
    /// Select every row from a table.
    pub fn each<S>() -> Query<S>
    where
        S: TableProjection,
    {
        let project = S::exprs("t0");
        Query::new(Select::new(
            S::select(&project),
            vec![Source::table("t0", S::qualified_name())],
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Select {
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

    fn to_sql(&self) -> String {
        let mut sql = Vec::new();
        self.write_sql(&mut sql)
            .expect("writing SQL to a Vec should not fail");
        String::from_utf8(sql).expect("generated SQL should be valid UTF-8")
    }

    fn write_sql(&self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(b"SELECT ")?;
        write_select(writer, &self.columns)?;
        writer.write_all(b" ")?;
        write_sources(writer, &self.sources)?;
        write_filters(writer, &self.filters)?;
        write_orders(writer, &self.orders)?;
        write_limit(writer, self.limit)?;
        write_offset(writer, self.offset)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Source {
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

    fn join(alias: impl Into<String>, table: impl ToString, on: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::InnerJoin { on: on.into() },
            target: SourceTarget::Table(table.to_string()),
        }
    }

    fn left_join(alias: impl Into<String>, table: impl ToString, on: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::LeftJoin { on: on.into() },
            target: SourceTarget::Table(table.to_string()),
        }
    }

    fn write_sql(&self, writer: &mut impl Write, position: usize) -> io::Result<()> {
        match (&self.kind, &self.target, position) {
            (SourceKind::From, SourceTarget::Table(table), _) => {
                write!(writer, "FROM {table} AS {}", self.alias)
            }
            (SourceKind::From, SourceTarget::Query(query), _) => {
                writer.write_all(b"FROM (")?;
                query.write_sql(writer)?;
                write!(writer, ") AS {}", self.alias)
            }
            (SourceKind::InnerLateral, SourceTarget::Query(query), 0) => {
                writer.write_all(b"FROM (")?;
                query.write_sql(writer)?;
                write!(writer, ") AS {}", self.alias)
            }
            (SourceKind::InnerLateral, SourceTarget::Query(query), _) => {
                writer.write_all(b"INNER JOIN LATERAL (")?;
                query.write_sql(writer)?;
                write!(writer, ") AS {} ON TRUE", self.alias)
            }
            (SourceKind::InnerLateral, SourceTarget::Table(table), 0) => {
                write!(writer, "FROM {table} AS {}", self.alias)
            }
            (SourceKind::InnerLateral, SourceTarget::Table(table), _) => {
                write!(
                    writer,
                    "INNER JOIN LATERAL {table} AS {} ON TRUE",
                    self.alias
                )
            }
            (SourceKind::InnerJoin { on: _ }, SourceTarget::Table(table), 0) => {
                write!(writer, "FROM {table} AS {}", self.alias)
            }
            (SourceKind::InnerJoin { on }, SourceTarget::Table(table), _) => {
                write!(writer, "INNER JOIN {table} AS {} ON {on}", self.alias)
            }
            (SourceKind::InnerJoin { on: _ }, SourceTarget::Query(query), 0) => {
                writer.write_all(b"FROM (")?;
                query.write_sql(writer)?;
                write!(writer, ") AS {}", self.alias)
            }
            (SourceKind::InnerJoin { on }, SourceTarget::Query(query), _) => {
                writer.write_all(b"INNER JOIN (")?;
                query.write_sql(writer)?;
                write!(writer, ") AS {} ON {on}", self.alias)
            }
            (SourceKind::LeftJoin { on: _ }, SourceTarget::Table(table), 0) => {
                write!(writer, "FROM {table} AS {}", self.alias)
            }
            (SourceKind::LeftJoin { on }, SourceTarget::Table(table), _) => {
                write!(writer, "LEFT JOIN {table} AS {} ON {on}", self.alias)
            }
            (SourceKind::LeftJoin { on: _ }, SourceTarget::Query(query), 0) => {
                writer.write_all(b"FROM (")?;
                query.write_sql(writer)?;
                write!(writer, ") AS {}", self.alias)
            }
            (SourceKind::LeftJoin { on }, SourceTarget::Query(query), _) => {
                writer.write_all(b"LEFT JOIN (")?;
                query.write_sql(writer)?;
                write!(writer, ") AS {} ON {on}", self.alias)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceKind {
    From,
    InnerLateral,
    InnerJoin { on: String },
    LeftJoin { on: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceTarget {
    Table(String),
    Query(Box<Select>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Filter {
    predicate: String,
}

impl Filter {
    fn new(predicate: impl Into<String>) -> Self {
        Self {
            predicate: predicate.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Sort {
    order: String,
}

impl Sort {
    fn new(order: impl Into<String>) -> Self {
        Self {
            order: order.into(),
        }
    }
}

/// Scoped query builder. Each `q` call adds a lateral-capable subquery.
pub struct Q<'scope> {
    depth: usize,
    sources: Vec<Source>,
    filters: Vec<Filter>,
    orders: Vec<Sort>,
    limit: Option<usize>,
    offset: Option<usize>,
    _phantom: PhantomData<&'scope ()>,
}

impl<'scope> Q<'scope> {
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
            .push(Source::join(alias, S::qualified_name(), predicate.to_sql()));
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
            predicate.to_sql(),
        ));
        projection
    }

    /// Add a subquery as a lateral source and return its projected expression columns in this query scope.
    pub fn lateral<Shape>(&mut self, query: Query<Shape>) -> Shape::Exprs<'scope>
    where
        Shape: ProjectionShape,
    {
        let alias = self.next_alias();
        self.sources
            .push(Source::lateral(alias.clone(), query.select));
        Shape::exprs(&alias)
    }

    /// Add a subquery and return its projected expression columns in this query scope.
    pub fn q<Shape>(&mut self, query: Query<Shape>) -> Shape::Exprs<'scope>
    where
        Shape: ProjectionShape,
    {
        self.lateral(query)
    }

    fn next_alias(&self) -> String {
        format!("q{}_{}", self.depth, self.sources.len())
    }

    /// Add a SQL `WHERE` predicate to the query currently being built.
    pub fn where_(&mut self, predicate: Predicate<'scope>) {
        self.filters.push(Filter::new(predicate.to_sql()));
    }

    /// Add an `ORDER BY` expression to the query currently being built.
    pub fn order_by(&mut self, order: Order<'scope>) {
        self.orders.push(Sort::new(order.to_sql()));
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

/// Build a query from a scoped query builder closure.
///
/// Expressions created by the builder are scoped to that builder invocation and cannot be
/// smuggled out as reusable values:
///
/// ```compile_fail
/// use squealy::*;
///
/// #[derive(Clone, Table)]
/// struct User<'scope, C: ColumnMode = ColumnExpr> {
///     id: C::Type<'scope, i32>,
/// }
///
/// let mut leaked = None;
/// let _ = query::<User>(|q| {
///     let user = q.each::<User>();
///     leaked = Some(user.clone());
///     user
/// });
/// let _ = leaked.unwrap();
/// ```
pub fn query<Shape>(
    f: impl for<'scope> FnOnce(&mut Q<'scope>) -> <Shape as ProjectionShape>::Exprs<'scope>,
) -> Query<Shape>
where
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

        Query::new(select)
    })
}

fn write_select(writer: &mut impl Write, columns: &[SelectColumn]) -> io::Result<()> {
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write!(writer, "{} AS {}", column.expr, column.alias)?;
    }

    Ok(())
}

fn write_sources(writer: &mut impl Write, sources: &[Source]) -> io::Result<()> {
    for (index, source) in sources.iter().enumerate() {
        if index > 0 {
            writer.write_all(b" ")?;
        }
        source.write_sql(writer, index)?;
    }

    Ok(())
}

fn write_filters(writer: &mut impl Write, filters: &[Filter]) -> io::Result<()> {
    if filters.is_empty() {
        return Ok(());
    }

    writer.write_all(b" WHERE ")?;
    for (index, filter) in filters.iter().enumerate() {
        if index > 0 {
            writer.write_all(b" AND ")?;
        }
        writer.write_all(filter.predicate.as_bytes())?;
    }

    Ok(())
}

fn write_orders(writer: &mut impl Write, orders: &[Sort]) -> io::Result<()> {
    if orders.is_empty() {
        return Ok(());
    }

    writer.write_all(b" ORDER BY ")?;
    for (index, order) in orders.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        writer.write_all(order.order.as_bytes())?;
    }

    Ok(())
}

fn write_limit(writer: &mut impl Write, limit: Option<usize>) -> io::Result<()> {
    if let Some(limit) = limit {
        write!(writer, " LIMIT {limit}")?;
    }

    Ok(())
}

fn write_offset(writer: &mut impl Write, offset: Option<usize>) -> io::Result<()> {
    if let Some(offset) = offset {
        write!(writer, " OFFSET {offset}")?;
    }

    Ok(())
}
