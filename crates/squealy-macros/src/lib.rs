//! Procedural macros for squealy.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;

mod column_type;
mod common;
mod cte;
mod database;
mod expr_parser;
mod expr_tokens;
mod schema;
mod table;
mod tuple;
mod view;

/// Derives squealy table metadata for generic table-mode structs.
#[proc_macro_derive(
	Table,
	attributes(
		column,
		primary_key,
		index,
		unique,
		nullable,
		not_null,
		auto_increment,
		default,
		default_raw,
		db_type,
		references,
		check,
		column_name,
		schema
	)
)]
pub fn derive_table(input: TokenStream) -> TokenStream {
	table::derive(input)
}

/// Derives transparent column metadata for single-field newtype wrappers.
///
/// The wrapper uses its inner field's column type, bind conversion, row decoding,
/// and literal expression behavior. Add `#[column_type(db_type = "...")]` when the
/// wrapper should keep the transparent Rust conversions but render a raw
/// backend-specific DDL type.
#[proc_macro_derive(ColumnType, attributes(column_type))]
pub fn derive_column_type(input: TokenStream) -> TokenStream {
	column_type::derive(input)
}

/// Derives squealy schema metadata from fields containing table types. A field marked `#[view]`
/// registers as a view instead of a table.
#[proc_macro_derive(Schema, attributes(view))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
	schema::derive(input)
}

/// Derives [`SchemaView`](squealy::SchemaView) for a view-mode struct: its declared output columns,
/// name, and namespace. The view body is supplied separately via `ViewDefinition::definition`.
#[proc_macro_derive(View, attributes(column, column_name, schema))]
pub fn derive_view(input: TokenStream) -> TokenStream {
	view::derive(input)
}

/// Derives squealy database metadata from fields containing schema types.
/// Derives [`SchemaCte`](squealy::SchemaCte) for a CTE struct: its declared output columns, name, and
/// the read-only queryable projection. The CTE body is supplied separately via
/// `CteDefinition::definition`, and the CTE is inlined as a `WITH` clause when referenced.
#[proc_macro_derive(CTE, attributes(column, column_name, schema))]
pub fn derive_cte(input: TokenStream) -> TokenStream {
	cte::derive(input)
}

/// Derives a recursive CTE (`WITH RECURSIVE`): same projection/metadata as `#[derive(CTE)]`, but the
/// body is `<anchor> UNION [ALL] <recursive>` supplied via `RecursiveCteDefinition::definition`.
#[proc_macro_derive(RecursiveCTE, attributes(column, column_name, schema))]
pub fn derive_recursive_cte(input: TokenStream) -> TokenStream {
	cte::derive_recursive(input)
}

#[proc_macro_derive(Database)]
pub fn derive_database(input: TokenStream) -> TokenStream {
	database::derive(input)
}

/// Generates projection support for tuple shapes from arity 2 through the supplied maximum.
#[proc_macro]
pub fn tuple_projection_shapes(input: TokenStream) -> TokenStream {
	tuple::projection_shapes(input)
}

/// Generates fixed homogeneous list support for tuple arities from 1 through the supplied maximum.
#[proc_macro]
pub fn tuple_fixed_lists(input: TokenStream) -> TokenStream {
	tuple::fixed_lists(input)
}

/// Generates conversions from HLists to tuples from arity 1 through the supplied maximum.
#[proc_macro]
pub fn hlist_tuples(input: TokenStream) -> TokenStream {
	tuple::hlist_tuples(input)
}

/// Generates prepared parameter tuple support for arities from 1 through the supplied maximum.
#[proc_macro]
pub fn prepared_param_values(input: TokenStream) -> TokenStream {
	tuple::prepared_param_values(input)
}

/// Generates explicit insert column/value tuple support from arity 1 through the supplied maximum.
#[proc_macro]
pub fn insert_column_values(input: TokenStream) -> TokenStream {
	tuple::insert_column_values(input)
}

/// Generates explicit update column/value tuple support from arity 1 through the supplied maximum.
#[proc_macro]
pub fn update_column_values(input: TokenStream) -> TokenStream {
	tuple::update_column_values(input)
}
