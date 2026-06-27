use proc_macro::TokenStream;
use proc_macro2::{Literal, Span};

use crate::common::to_snake_plural;
use crate::table::{TableStruct, table_struct};

/// Derives [`SchemaCte`](squealy::SchemaCte) for a CTE struct: its declared output columns, name, and
/// the read-only queryable projection (so the CTE is referenceable as a `FROM` source) — mirroring
/// `#[derive(View)]`. The CTE body is supplied separately via `CteDefinition::definition`.
pub(crate) fn derive(input: TokenStream) -> TokenStream {
    match table_struct(input) {
        Ok(cte) => {
            // Same read-only projection machinery as a view (`is_view = true`: no write-side impls),
            // plus the CTE's own `SchemaCte` metadata.
            let projection: proc_macro2::TokenStream = cte.expand(true).into();
            let schema_cte: proc_macro2::TokenStream = expand(&cte).into();
            quote::quote! { #projection #schema_cte }.into()
        }
        Err(error) => error.into_compile_error(),
    }
}

fn expand(cte: &TableStruct) -> TokenStream {
    let ident = proc_macro2::Ident::new(&cte.ident.to_string(), Span::call_site());
    let cte_name = Literal::string(&to_snake_plural(&ident.to_string()));

    let column_names = cte
        .fields
        .iter()
        .map(|field| Literal::string(&field.column_name()))
        .collect::<Vec<_>>();
    // The declared value type `D` of each column (e.g. `i32` / `Option<String>`): the projection's row
    // element and the source of the column's SQL type + nullability.
    let value_tys = cte
        .fields
        .iter()
        .map(|field| field.value_ty.clone())
        .collect::<Vec<_>>();

    quote::quote! {
        impl<'scope, C: ::squealy::ColumnMode> ::squealy::SchemaCte for #ident <'scope, C> {
            // The ordered column types; the `impl ViewSelect<Row = Self::Row>` bound on
            // `CteDefinition::definition` checks the body's projection decodes to exactly this.
            type Row = ( #( #value_tys, )* );

            fn cte_name() -> &'static str {
                #cte_name
            }

            fn cte_columns() -> ::std::vec::Vec<::squealy::ViewColumnModel> {
                ::std::vec![ #(
                    ::squealy::ViewColumnModel {
                        name: #column_names.to_owned(),
                        ty: <<#value_tys as ::squealy::ColumnNullability>::Inner
                            as ::squealy::HasColumnType>::COLUMN_TYPE.into(),
                        nullable: <#value_tys as ::squealy::ColumnNullability>::NULLABLE,
                    },
                )* ]
            }
        }
    }
    .into()
}
