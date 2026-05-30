use std::borrow::Cow;

use crate::{Column, ColumnMode, ColumnName, Connection, Index, Projectable, Schema};

/// Object-safe table metadata exposed through schema membership.
pub trait Table {
    fn schema_name(&self) -> Option<&'static str>;

    fn name(&self) -> &'static str;

    fn qualified_name(&self) -> Cow<'static, str> {
        match self.schema_name() {
            Some(schema) => Cow::Owned(format!("{schema}.{}", self.name())),
            None => Cow::Borrowed(self.name()),
        }
    }

    fn columns(&self) -> &'static [&'static dyn Column];

    fn indexes(&self) -> &'static [&'static dyn Index];
}

/// A typed table model that can also project expression columns.
pub trait SchemaTable: Table {
    type Schema: Schema;

    type WithColumn<'scope, C: ColumnMode>
    where
        C: 'scope;

    type Exprs<'scope>: Projectable;

    type NullableExprs<'scope>: Projectable;

    /// Returns the containing schema namespace for this model, if one is configured.
    fn schema_name() -> Option<&'static str>
    where
        Self: Sized,
    {
        <Self::Schema as Schema>::name()
    }

    /// Returns the default table name for this model.
    fn name() -> &'static str
    where
        Self: Sized;

    /// Returns the schema-qualified table name for this model.
    fn qualified_name() -> Cow<'static, str>
    where
        Self: Sized,
    {
        match <Self as SchemaTable>::schema_name() {
            Some(schema) => Cow::Owned(format!("{schema}.{}", <Self as SchemaTable>::name())),
            None => Cow::Borrowed(<Self as SchemaTable>::name()),
        }
    }

    /// Returns the table's database column schema metadata.
    fn columns() -> &'static [&'static dyn Column]
    where
        Self: Sized;

    /// Returns the table's database index schema metadata.
    fn indexes() -> &'static [&'static dyn Index]
    where
        Self: Sized;

    /// Returns the database column names for this model.
    fn column_names() -> Self::WithColumn<'static, ColumnName>;

    /// Build expression-mode fields that refer to the supplied SQL alias.
    fn column_exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
        Self::column_exprs_from(alias, &Self::column_names())
    }

    /// Build expression-mode fields from explicit database column names.
    fn column_exprs_from<'scope>(
        alias: &str,
        columns: &Self::WithColumn<'static, ColumnName>,
    ) -> Self::Exprs<'scope>;

    /// Build nullable expression-mode fields that refer to the supplied SQL alias.
    fn nullable_column_exprs<'scope>(alias: &str) -> Self::NullableExprs<'scope> {
        Self::nullable_column_exprs_from(alias, &Self::column_names())
    }

    /// Build nullable expression-mode fields from explicit database column names.
    fn nullable_column_exprs_from<'scope>(
        alias: &str,
        columns: &Self::WithColumn<'static, ColumnName>,
    ) -> Self::NullableExprs<'scope>;
}

/// A table model whose value-mode fields can be inserted as bind parameters.
pub trait InsertableTable: SchemaTable {
    type InsertBuilder<'conn, Conn>
    where
        Conn: Connection + 'conn;

    fn insert_builder<'conn, Conn>(connection: &'conn Conn) -> Self::InsertBuilder<'conn, Conn>
    where
        Conn: Connection + 'conn;
}
