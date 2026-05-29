use std::borrow::Cow;

use crate::Expr;

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

    fn default(&self) -> Option<&'static str> {
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

/// Database schema metadata for a foreign-key reference.
pub trait ForeignKey: Sync {
    fn schema_name(&self) -> Option<&'static str> {
        None
    }

    fn table(&self) -> &'static str;

    fn column(&self) -> &'static str;

    fn on_delete(&self) -> Option<&'static str> {
        None
    }

    fn on_update(&self) -> Option<&'static str> {
        None
    }
}

/// Database schema metadata for an index.
pub trait Index: Sync {
    fn name(&self) -> Option<&'static str> {
        None
    }

    fn columns(&self) -> &'static [&'static str];

    fn unique(&self) -> bool {
        false
    }
}

/// A selected SQL expression and its output alias.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectColumn {
    pub expr: String,
    pub alias: Cow<'static, str>,
}

impl SelectColumn {
    pub fn new(expr: impl Into<String>, alias: impl Into<Cow<'static, str>>) -> Self {
        Self {
            expr: expr.into(),
            alias: alias.into(),
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
                .map(|column| SelectColumn::new(column.expr, prefix_alias("left", &column.alias))),
        );
        columns.extend(
            self.1
                .project()
                .into_iter()
                .map(|column| SelectColumn::new(column.expr, prefix_alias("right", &column.alias))),
        );
        columns
    }

    fn re_alias(&self, alias: &str) -> Self {
        (self.0.re_alias(alias), self.1.re_alias(alias))
    }
}

fn prefix_alias(prefix: &str, alias: &str) -> String {
    format!("{prefix}_{alias}")
}

/// A database schema namespace that can contain tables.
pub trait Schema {
    fn name() -> Option<&'static str>;
}

/// The default schema namespace for backends that do not need explicit qualification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultSchema {}

impl Schema for DefaultSchema {
    fn name() -> Option<&'static str> {
        None
    }
}

/// A database table model.
pub trait Table {
    type Schema: Schema;

    type WithColumn<'scope, C: ColumnMode>
    where
        C: 'scope;

    /// Returns the containing schema namespace for this model, if one is configured.
    fn schema_name() -> Option<&'static str> {
        <Self::Schema as Schema>::name()
    }

    /// Returns the default table name for this model.
    fn name() -> &'static str;

    /// Returns the schema-qualified table name for this model.
    fn qualified_name() -> Cow<'static, str> {
        match Self::schema_name() {
            Some(schema) => Cow::Owned(format!("{schema}.{}", Self::name())),
            None => Cow::Borrowed(Self::name()),
        }
    }

    /// Returns the table's database column schema metadata.
    fn columns() -> &'static [&'static dyn Column];

    /// Returns the table's database index schema metadata.
    fn indexes() -> &'static [&'static dyn Index];

    /// Returns the database column names for this model.
    fn column_names() -> Self::WithColumn<'static, ColumnName>;

    /// Build expression-mode fields that refer to the supplied SQL alias.
    fn column_exprs<'scope>(alias: &str) -> Self::WithColumn<'scope, ColumnExpr> {
        Self::column_exprs_from(alias, &Self::column_names())
    }

    /// Build expression-mode fields from explicit database column names.
    fn column_exprs_from<'scope>(
        alias: &str,
        columns: &Self::WithColumn<'static, ColumnName>,
    ) -> Self::WithColumn<'scope, ColumnExpr>;
}
