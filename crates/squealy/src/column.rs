use crate::{Expr, ForeignKey};

/// Controls how table fields are represented.
pub trait ColumnMode {
    type Type<'scope, U>;
}

/// Table fields are typed SQL expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnExpr {}

impl ColumnMode for ColumnExpr {
    type Type<'scope, U> = Expr<'scope, U>;
}

/// Table fields are database column names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnName {}

impl ColumnMode for ColumnName {
    type Type<'scope, U> = &'static str;
}

/// Table fields are plain Rust values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnValue {}

impl ColumnMode for ColumnValue {
    type Type<'scope, U> = U;
}

/// Table fields are nullable Rust values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnNullableValue {}

impl ColumnMode for ColumnNullableValue {
    type Type<'scope, U> = Option<U>;
}

/// A backend-agnostic column default.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnDefault {
    Null,
    Int(i128),
    UInt(u128),
    Float(f64),
    Text(&'static str),
    Bool(bool),
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
    Raw(&'static str),
}

/// Database schema metadata for a single column.
pub trait Column: Sync {
    fn name(&self) -> &'static str;

    fn primary_key(&self) -> bool {
        false
    }

    fn indexed(&self) -> bool {
        false
    }

    fn unique(&self) -> bool {
        false
    }

    fn nullable(&self) -> bool {
        false
    }

    fn auto_increment(&self) -> bool {
        false
    }

    fn generated(&self) -> bool {
        false
    }

    fn insertable(&self) -> bool {
        !self.generated() && !self.auto_increment()
    }

    fn updateable(&self) -> bool {
        !self.generated() && !self.auto_increment()
    }

    fn default(&self) -> Option<ColumnDefault> {
        None
    }

    fn db_type(&self) -> Option<&'static str> {
        None
    }

    fn check(&self) -> Option<&'static str> {
        None
    }

    fn references(&self) -> Option<&'static dyn ForeignKey> {
        None
    }
}
