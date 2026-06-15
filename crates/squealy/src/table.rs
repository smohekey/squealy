use std::borrow::Cow;

use crate::{
    Column, ColumnMode, ColumnName, Index, Projectable, QueryBuilder, Schema, SourceAlias,
};

/// A table-level primary key declared with the `#[primary_key(columns = [..])]` attribute.
///
/// Unlike an [`Index`], a table has at most one primary key, so it is exposed as a small
/// `Copy` value rather than a trait object. When `name` is `None` the model builder falls
/// back to the deterministic `pk_<table>` convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TablePrimaryKey {
    pub name: Option<&'static str>,
    pub columns: &'static [&'static str],
}

/// A table-level unique constraint declared with the `#[unique(columns = [..])]` attribute.
///
/// Unlike [`TablePrimaryKey`], a table may declare several of these, so they are exposed as a
/// slice. The single-column `#[column(unique)]` form is handled separately by the model builder;
/// this type carries only the composite / table-level declarations. When `name` is `None` the
/// model builder falls back to the deterministic `uq_<table>_<columns>` convention.
///
/// A `predicate` is present when the constraint is declared with a `where = |row| ...` filter.
/// Such a unique is lowered to a *partial unique index* rather than a table `UNIQUE` constraint
/// (Postgres cannot attach a `WHERE` to a table constraint); the function lowers the typed
/// predicate to an ANSI SQL string when the model is built.
// `PartialEq`/`Eq` compare the predicate by function-pointer identity, which is not meaningful on
// its own; nothing relies on `TableUnique` equality (the model builder reads the fields), so the
// derive is kept only for parity with the other metadata structs and the lint is silenced.
#[allow(unpredictable_function_pointer_comparisons)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableUnique {
    pub name: Option<&'static str>,
    pub columns: &'static [&'static str],
    pub predicate: Option<fn() -> String>,
}

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

    /// Returns the table-level primary key declared on this model, if any. Tables that mark the
    /// primary key with per-column `#[column(primary_key)]` return `None` here; that case is
    /// hoisted into a constraint by the model builder instead.
    fn primary_key(&self) -> Option<TablePrimaryKey> {
        None
    }

    /// Returns the composite unique constraints declared with `#[unique(columns = [..])]`.
    /// Single-column `#[column(unique)]` markers are not reported here; they are hoisted into
    /// constraints by the model builder instead.
    fn uniques(&self) -> &'static [TableUnique] {
        &[]
    }
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

    /// Returns the table-level primary key declared on this model, if any. See
    /// [`Table::primary_key`] for how per-column primary keys are handled instead.
    fn primary_key() -> Option<TablePrimaryKey>
    where
        Self: Sized,
    {
        None
    }

    /// Returns the composite unique constraints declared on this model. See [`Table::uniques`]
    /// for how single-column `#[column(unique)]` markers are handled instead.
    fn uniques() -> &'static [TableUnique]
    where
        Self: Sized,
    {
        &[]
    }

    /// Returns the database column names for this model.
    fn column_names() -> Self::WithColumn<'static, ColumnName>;

    /// Build expression-mode fields that refer to the supplied SQL alias.
    fn column_exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
        Self::column_exprs_from(alias, &Self::column_names())
    }

    /// Build expression-mode fields from explicit database column names.
    fn column_exprs_from<'scope>(
        alias: SourceAlias,
        columns: &Self::WithColumn<'static, ColumnName>,
    ) -> Self::Exprs<'scope>;

    /// Build nullable expression-mode fields that refer to the supplied SQL alias.
    fn nullable_column_exprs<'scope>(alias: SourceAlias) -> Self::NullableExprs<'scope> {
        Self::nullable_column_exprs_from(alias, &Self::column_names())
    }

    /// Build nullable expression-mode fields from explicit database column names.
    fn nullable_column_exprs_from<'scope>(
        alias: SourceAlias,
        columns: &Self::WithColumn<'static, ColumnName>,
    ) -> Self::NullableExprs<'scope>;
}

/// A table model whose value-mode fields can be inserted as bind parameters.
pub trait InsertableTable: SchemaTable {}

/// A table model that can generate a typed update builder.
pub trait UpdateableTable: SchemaTable {}

/// A table model that can generate a typed write builder for insert and update.
pub trait WriteableTable: SchemaTable {
    type WriteBuilder<'conn, Conn>
    where
        Conn: QueryBuilder + 'conn;

    fn write_builder<'conn, Conn>(connection: &'conn Conn) -> Self::WriteBuilder<'conn, Conn>
    where
        Conn: QueryBuilder + 'conn;
}
