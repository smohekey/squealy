//! Procedural macros for squealy.

use proc_macro::TokenStream;

mod common;
mod database;
mod schema;
mod table;
mod tuple;

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

/// Derives squealy schema metadata from fields containing table types.
#[proc_macro_derive(Schema)]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    schema::derive(input)
}

/// Derives squealy database metadata from fields containing schema types.
#[proc_macro_derive(Database)]
pub fn derive_database(input: TokenStream) -> TokenStream {
    database::derive(input)
}

/// Generates projection support for tuple shapes from arity 2 through the supplied maximum.
#[proc_macro]
pub fn tuple_projection_shapes(input: TokenStream) -> TokenStream {
    tuple::projection_shapes(input)
}
