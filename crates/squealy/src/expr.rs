use std::fmt::Display;
use std::marker::PhantomData;

/// Marker trait for Rust types that can participate in numeric SQL operations.
pub trait SqlNumber {}

macro_rules! impl_sql_number {
    ($($ty:ty),* $(,)?) => {
        $(impl SqlNumber for $ty {})*
    };
}

impl_sql_number!(i8, i16, i32, i64, i128, isize);
impl_sql_number!(u8, u16, u32, u64, u128, usize);
impl_sql_number!(f32, f64);

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
    pub fn equals(self, other: Self) -> Predicate<'scope> {
        Predicate::new(format!("({} = {})", self.sql, other.sql))
    }

    /// SQL inequality.
    pub fn not_equals(self, other: Self) -> Predicate<'scope> {
        Predicate::new(format!("({} <> {})", self.sql, other.sql))
    }

    /// SQL less-than comparison.
    pub fn less_than(self, other: Self) -> Predicate<'scope> {
        Predicate::new(format!("({} < {})", self.sql, other.sql))
    }

    /// SQL less-than-or-equal comparison.
    pub fn less_than_or_equals(self, other: Self) -> Predicate<'scope> {
        Predicate::new(format!("({} <= {})", self.sql, other.sql))
    }

    /// SQL greater-than comparison.
    pub fn greater_than(self, other: Self) -> Predicate<'scope> {
        Predicate::new(format!("({} > {})", self.sql, other.sql))
    }

    /// SQL greater-than-or-equal comparison.
    pub fn greater_than_or_equals(self, other: Self) -> Predicate<'scope> {
        Predicate::new(format!("({} >= {})", self.sql, other.sql))
    }
}

impl<'scope, T> Expr<'scope, T>
where
    T: SqlNumber,
{
    /// SQL numeric addition.
    pub fn add(self, other: Self) -> Self {
        Self::new(format!("({} + {})", self.sql, other.sql))
    }

    /// SQL numeric subtraction.
    pub fn subtract(self, other: Self) -> Self {
        Self::new(format!("({} - {})", self.sql, other.sql))
    }
}

impl<'scope, T> Clone for Expr<'scope, T> {
    fn clone(&self) -> Self {
        Self::new(self.sql.clone())
    }
}

/// A typed SQL boolean predicate scoped to a query builder invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct Predicate<'scope> {
    sql: String,
    _phantom: PhantomData<&'scope ()>,
}

impl<'scope> Predicate<'scope> {
    pub(crate) fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            _phantom: PhantomData,
        }
    }

    /// Render this predicate as SQL.
    pub fn to_sql(&self) -> &str {
        &self.sql
    }

    /// SQL conjunction.
    pub fn and(self, other: Self) -> Self {
        Self::new(format!("({} AND {})", self.sql, other.sql))
    }

    /// SQL disjunction.
    pub fn or(self, other: Self) -> Self {
        Self::new(format!("({} OR {})", self.sql, other.sql))
    }

    /// SQL negation.
    pub fn not_(self) -> Self {
        Self::new(format!("(NOT {})", self.sql))
    }
}

impl<'scope> Clone for Predicate<'scope> {
    fn clone(&self) -> Self {
        Self::new(self.sql.clone())
    }
}
