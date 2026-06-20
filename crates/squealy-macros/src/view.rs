use proc_macro::TokenStream;
use proc_macro2::{Literal, Span};

use crate::common::to_snake_plural;
use crate::table::{TableStruct, table_struct};

/// Derives [`SchemaView`](squealy::SchemaView) for a view struct: its declared output columns, name,
/// and namespace, mirroring how `#[derive(Table)]` derives a table's metadata. The user supplies the
/// body separately by implementing `ViewDefinition::definition`.
pub(crate) fn derive(input: TokenStream) -> TokenStream {
    match table_struct(input) {
        Ok(view) => expand(&view),
        Err(error) => error.into_compile_error(),
    }
}

fn expand(view: &TableStruct) -> TokenStream {
    let ident = proc_macro2::Ident::new(&view.ident.to_string(), Span::call_site());
    let view_name = Literal::string(&to_snake_plural(&ident.to_string()));
    let schema_ty = view
        .schema
        .clone()
        .unwrap_or_else(|| quote::quote! { ::squealy::DefaultSchema });

    let column_names = view
        .fields
        .iter()
        .map(|field| Literal::string(&field.column_name()))
        .collect::<Vec<_>>();
    // The declared value type `D` of each column (e.g. `i32` or `Option<String>`). It is both the
    // projection's row element and the source of the column's SQL type and nullability.
    let value_tys = view
        .fields
        .iter()
        .map(|field| field.value_ty.clone())
        .collect::<Vec<_>>();

    quote::quote! {
        impl<'scope, C: ::squealy::ColumnMode> ::squealy::SchemaView for #ident <'scope, C> {
            // The ordered column types; the `impl ViewSelect<Row = Self::Row>` bound on
            // `ViewDefinition::definition` checks the body's projection decodes to exactly this.
            type Row = ( #( #value_tys, )* );

            fn schema_name() -> ::std::option::Option<&'static str> {
                <#schema_ty as ::squealy::Schema>::name()
            }

            fn view_name() -> &'static str {
                #view_name
            }

            fn view_columns() -> ::std::vec::Vec<::squealy::ViewColumnModel> {
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
