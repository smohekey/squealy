use std::fmt::Display;
use std::marker::PhantomData;

/// A typed SQL scalar expression scoped to a query builder invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct Expr<'scope, T> {
    sql: String,
    _phantom: PhantomData<(&'scope (), T)>,
}

impl<'scope, T> Expr<'scope, T> {
    pub(crate) fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn column(alias: &str, column: &str) -> Self {
        Self::new(format!("{alias}.{column}"))
    }

    /// Construct a SQL literal expression.
    pub fn lit(value: impl Display) -> Self {
        Self::new(value.to_string())
    }

    /// Render this expression as SQL.
    pub fn to_sql(&self) -> &str {
        &self.sql
    }

    /// SQL equality.
    pub fn equals(self, other: Self) -> Expr<'scope, bool> {
        Expr::new(format!("({} = {})", self.sql, other.sql))
    }

    /// SQL numeric addition.
    pub fn add(self, other: Self) -> Self {
        Self::new(format!("({} + {})", self.sql, other.sql))
    }
}

impl<'scope, T> Clone for Expr<'scope, T> {
    fn clone(&self) -> Self {
        Self::new(self.sql.clone())
    }
}
