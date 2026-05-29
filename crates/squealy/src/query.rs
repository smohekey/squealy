use std::cell::Cell;
use std::marker::PhantomData;

use crate::{Expr, Projectable, SelectColumn, Table};

/// A SQL select statement that produces rows with shape `T`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query<T> {
    sql: String,
    project: T,
}

impl<T> Query<T> {
    pub(crate) fn new(sql: impl Into<String>, project: T) -> Self {
        Self {
            sql: sql.into(),
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
    pub fn each<S>() -> Query<<S as Table>::WithMode<'static, crate::ExprMode>>
    where
        S: Table,
        <S as Table>::WithMode<'static, crate::ExprMode>: Projectable,
    {
        let project = S::columns("t0");
        let select = render_select(project.project());
        Query::new(format!("SELECT {select} FROM {} AS t0", S::name()), project)
    }
}

/// Scoped query builder. Each `q` call adds a lateral-capable subquery.
pub struct Q<'scope> {
    depth: usize,
    sources: Vec<(String, String)>,
    filters: Vec<String>,
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

    /// Add a subquery and return its projected expression columns in this query scope.
    pub fn q<T>(&mut self, query: Query<T>) -> T
    where
        T: Projectable,
    {
        let alias = format!("q{}_{}", self.depth, self.sources.len());
        self.sources.push((alias.clone(), query.sql));
        query.project.re_alias(&alias)
    }

    /// Add a SQL `WHERE` predicate to the query currently being built.
    pub fn where_(&mut self, expr: Expr<'scope, bool>) {
        self.filters.push(expr.to_sql().to_owned());
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

        let select = render_select(output.project());
        let from = render_sources(q.sources);
        let filters = render_filters(q.filters);

        Query::new(format!("SELECT {select} {from}{filters}"), output)
    })
}

fn render_select(columns: Vec<SelectColumn>) -> String {
    columns
        .into_iter()
        .map(|column| format!("{} AS {}", column.expr, column.alias))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_sources(sources: Vec<(String, String)>) -> String {
    let mut iter = sources.into_iter();
    let Some((first_alias, first_sql)) = iter.next() else {
        return String::new();
    };

    let mut sql = format!("FROM ({first_sql}) AS {first_alias}");
    for (alias, source_sql) in iter {
        sql.push_str(&format!(
            " INNER JOIN LATERAL ({source_sql}) AS {alias} ON TRUE"
        ));
    }
    sql
}

fn render_filters(filters: Vec<String>) -> String {
    if filters.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", filters.join(" AND "))
    }
}
