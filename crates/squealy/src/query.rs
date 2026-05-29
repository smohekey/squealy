use std::cell::Cell;
use std::marker::PhantomData;

use crate::{Predicate, Projectable, SchemaTable, SelectColumn};

/// A SQL select statement that produces rows with shape `T`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query<T> {
    select: Select,
    sql: String,
    project: T,
}

impl<T> Query<T> {
    fn new(select: Select, project: T) -> Self {
        let sql = select.to_sql();

        Self {
            select,
            sql,
            project,
        }
    }

    /// Render this query to SQL.
    pub fn to_sql(&self) -> &str {
        &self.sql
    }
}

impl<S> Query<S>
where
    S: Projectable,
{
    pub fn project(&self) -> &S {
        &self.project
    }
}

impl Query<()> {
    /// Select every row from a table.
    pub fn each<S>() -> Query<<S as SchemaTable>::WithColumn<'static, crate::ColumnExpr>>
    where
        S: SchemaTable,
        <S as SchemaTable>::WithColumn<'static, crate::ColumnExpr>: Projectable,
    {
        let project = S::column_exprs("t0");
        Query::new(
            Select::new(
                project.project(),
                vec![Source::table("t0", <S as SchemaTable>::qualified_name())],
            ),
            project,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Select {
    columns: Vec<SelectColumn>,
    sources: Vec<Source>,
    filters: Vec<Filter>,
}

impl Select {
    fn new(columns: Vec<SelectColumn>, sources: Vec<Source>) -> Self {
        Self {
            columns,
            sources,
            filters: Vec::new(),
        }
    }

    fn with_filters(mut self, filters: Vec<Filter>) -> Self {
        self.filters = filters;
        self
    }

    fn to_sql(&self) -> String {
        let select = render_select(&self.columns);
        let from = render_sources(&self.sources);
        let filters = render_filters(&self.filters);

        format!("SELECT {select} {from}{filters}")
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

    fn to_sql(&self, position: usize) -> String {
        match (&self.kind, &self.target, position) {
            (SourceKind::From, SourceTarget::Table(table), _) => {
                format!("FROM {table} AS {}", self.alias)
            }
            (SourceKind::From, SourceTarget::Query(query), _) => {
                format!("FROM ({}) AS {}", query.to_sql(), self.alias)
            }
            (SourceKind::InnerLateral, SourceTarget::Query(query), 0) => {
                format!("FROM ({}) AS {}", query.to_sql(), self.alias)
            }
            (SourceKind::InnerLateral, SourceTarget::Query(query), _) => format!(
                "INNER JOIN LATERAL ({}) AS {} ON TRUE",
                query.to_sql(),
                self.alias
            ),
            (SourceKind::InnerLateral, SourceTarget::Table(table), 0) => {
                format!("FROM {table} AS {}", self.alias)
            }
            (SourceKind::InnerLateral, SourceTarget::Table(table), _) => {
                format!("INNER JOIN LATERAL {table} AS {} ON TRUE", self.alias)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceKind {
    From,
    InnerLateral,
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

/// Scoped query builder. Each `q` call adds a lateral-capable subquery.
pub struct Q<'scope> {
    depth: usize,
    sources: Vec<Source>,
    filters: Vec<Filter>,
    _phantom: PhantomData<&'scope ()>,
}

impl<'scope> Q<'scope> {
    fn new(depth: usize) -> Self {
        Self {
            depth,
            sources: Vec::new(),
            filters: Vec::new(),
            _phantom: PhantomData,
        }
    }

    /// Add a table source and return its expression columns in this query scope.
    pub fn each<S>(&mut self) -> <S as SchemaTable>::WithColumn<'scope, crate::ColumnExpr>
    where
        S: SchemaTable,
        <S as SchemaTable>::WithColumn<'scope, crate::ColumnExpr>: Projectable,
    {
        let alias = self.next_alias();
        self.sources.push(Source::table(
            alias.clone(),
            <S as SchemaTable>::qualified_name(),
        ));
        S::column_exprs(&alias)
    }

    /// Add a subquery as a lateral source and return its projected expression columns in this query scope.
    pub fn lateral<T>(&mut self, query: Query<T>) -> T::Rebound<'scope>
    where
        T: Projectable,
    {
        let alias = self.next_alias();
        self.sources
            .push(Source::lateral(alias.clone(), query.select));
        query.project.re_alias(&alias)
    }

    /// Add a subquery and return its projected expression columns in this query scope.
    pub fn q<T>(&mut self, query: Query<T>) -> T::Rebound<'scope>
    where
        T: Projectable,
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
}

thread_local! {
    static QUERY_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Build a query from a scoped query builder closure.
pub fn query<T>(f: impl FnOnce(&mut Q<'static>) -> T) -> Query<T>
where
    T: Projectable,
{
    QUERY_DEPTH.with(|depth| {
        let current_depth = depth.get();
        depth.set(current_depth + 1);

        let mut q = Q::new(current_depth);
        let output = f(&mut q);

        depth.set(current_depth);

        let select = Select::new(output.project(), q.sources).with_filters(q.filters);

        Query::new(select, output)
    })
}

fn render_select(columns: &[SelectColumn]) -> String {
    columns
        .iter()
        .map(|column| format!("{} AS {}", column.expr, column.alias))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_sources(sources: &[Source]) -> String {
    sources
        .iter()
        .enumerate()
        .map(|(index, source)| source.to_sql(index))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_filters(filters: &[Filter]) -> String {
    if filters.is_empty() {
        String::new()
    } else {
        format!(
            " WHERE {}",
            filters
                .iter()
                .map(|filter| filter.predicate.as_str())
                .collect::<Vec<_>>()
                .join(" AND ")
        )
    }
}
