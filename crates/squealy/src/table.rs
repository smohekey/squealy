use crate::Expr;

/// Controls how table fields are represented.
pub trait Column {
    type T<'scope, U>;
}

/// Table fields are typed SQL expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnExpr {}

impl Column for ColumnExpr {
    type T<'scope, U> = Expr<'scope, U>;
}

/// Table fields are database column names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnName {}

impl Column for ColumnName {
    type T<'scope, U> = &'static str;
}

/// Table fields are plain Rust values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnValue {}

impl Column for ColumnValue {
    type T<'scope, U> = U;
}

/// A selected SQL expression and its output alias.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectColumn {
    pub expr: String,
    pub alias: &'static str,
}

impl SelectColumn {
    pub fn new(expr: impl Into<String>, alias: &'static str) -> Self {
        Self {
            expr: expr.into(),
            alias,
        }
    }
}

/// A table-shaped value whose expression columns can be projected or rebound to a SQL alias.
pub trait Projectable: Clone {
    fn project(&self) -> Vec<SelectColumn>;

    fn re_alias(&self, alias: &str) -> Self;
}

impl<L, R> Projectable for (L, R)
where
    L: Projectable,
    R: Projectable,
{
    fn project(&self) -> Vec<SelectColumn> {
        let mut columns = Vec::new();
        columns.extend(
            self.0
                .project()
                .into_iter()
                .map(|column| SelectColumn::new(column.expr, prefix_alias("left", column.alias))),
        );
        columns.extend(
            self.1
                .project()
                .into_iter()
                .map(|column| SelectColumn::new(column.expr, prefix_alias("right", column.alias))),
        );
        columns
    }

    fn re_alias(&self, alias: &str) -> Self {
        (self.0.re_alias(alias), self.1.re_alias(alias))
    }
}

fn prefix_alias(prefix: &str, alias: &str) -> &'static str {
    Box::leak(format!("{prefix}_{alias}").into_boxed_str())
}

/// A database table model.
pub trait Table {
    type WithMode<'scope, Mode: Column>
    where
        Mode: 'scope;

    /// Returns the default table name for this model.
    fn name() -> &'static str;

    /// Returns the database column names for this model.
    fn column_names() -> Self::WithMode<'static, ColumnName>;

    /// Build expression-mode fields that refer to the supplied SQL alias.
    fn columns<'scope>(alias: &str) -> Self::WithMode<'scope, ColumnExpr> {
        Self::columns_from(alias, &Self::column_names())
    }

    /// Build expression-mode fields from explicit database column names.
    fn columns_from<'scope>(
        alias: &str,
        columns: &Self::WithMode<'static, ColumnName>,
    ) -> Self::WithMode<'scope, ColumnExpr>;
}
