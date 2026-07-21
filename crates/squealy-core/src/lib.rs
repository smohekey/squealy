//! Backend-neutral schema metadata and typed query engine for Squealy.

#![forbid(unsafe_code)]

extern crate self as squealy;

mod backend;
mod column;
mod connection;
mod cte;
mod database;
mod dialect;
mod expr;
mod foreign_key;
mod index;
mod ir;
mod list;
mod model;
mod projection;
mod query;
#[doc(hidden)]
pub mod render;
mod schema;
mod table;
mod view;
pub mod view_render;

pub use backend::{
	Backend, Connect, Decode, DecodeNullable, Encode, ParamWriter, RowReader, SupportsDateTrunc,
	SupportsExtract, SupportsFullJoin, SupportsNamedWindow, SupportsReturning,
};
pub use column::{
	Column, ColumnDefault, ColumnExpr, ColumnMode, ColumnName, ColumnNullability,
	ColumnNullableValue, ColumnType, ColumnValue, HasColumnType,
};
pub use connection::{Connection, ConnectionWithTransaction, QueryBuilder};
pub use cte::{
	cte_definition_dependencies, cte_definition_model, recursive_cte_definition_body,
	recursive_cte_definition_dependencies, CteDef, CteDefinition, RecursiveBody,
	RecursiveCteDefinition, RecursiveUnion, SchemaCte,
};
pub use database::Database;
pub use dialect::{
	reject_128bit_general_cast, DeleteUsingStyle, Dialect, SetOperandStyle, UpdateFromStyle,
};
pub use expr::*;
pub use foreign_key::ForeignKey;
pub use index::Index;
pub use ir::*;
pub use list::*;
pub use model::{table_from_dyn, DatabaseModel, ViewDef};
pub use projection::*;
pub use query::*;
pub use schema::{DatabaseSchema, DefaultSchema, Schema};
pub use table::{
	InsertableTable, SchemaTable, Table, TablePrimaryKey, TableUnique, UpdateableTable,
	WriteableTable,
};
#[doc(hidden)]
pub use view::{build_ddl_predicate, lower_view, view_definition_model};
pub use view::{ModelBackend, ModelConn, SchemaView, ViewDefinition, ViewSelect};
pub use view_render::{
	ordered_views, render_create_view, render_drop_view, render_scalar_expr, render_with_prefix,
};

/// Returns the product name used by the retained executable.
#[must_use]
pub const fn name() -> &'static str {
	"squealy"
}
