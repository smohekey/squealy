use proc_macro::TokenStream;
use proc_macro2::{Literal, Span};

use crate::common::{generated_ident, to_snake_plural};
use crate::table::{SourceMode, TableStruct, table_struct};

/// Derives [`SchemaCte`](squealy::SchemaCte) for a CTE struct: its declared output columns, name, and
/// the read-only queryable projection (so the CTE is referenceable as a `FROM` source) — mirroring
/// `#[derive(View)]`. The CTE body is supplied separately via `CteDefinition::definition`, and the
/// CTE is inlined as a `WITH` clause when referenced.
pub(crate) fn derive(input: TokenStream) -> TokenStream {
    match table_struct(input) {
        Ok(mut cte) => {
            // A CTE is referenced by its bare `WITH` name, never schema-qualified: drop any schema so
            // the generated `SchemaTable`/`FROM` rendering emits an unqualified identifier.
            cte.schema = None;
            // Same read-only projection machinery as a view (no write-side impls), but in `Cte` mode
            // so the table macro does *not* emit a `QuerySource` impl — the CTE supplies its own below
            // (carrying its `CteDef`). Plus the CTE's `SchemaCte` metadata.
            let projection: proc_macro2::TokenStream = cte.expand(SourceMode::Cte).into();
            let schema_cte: proc_macro2::TokenStream = expand(&cte).into();
            quote::quote! { #projection #schema_cte }.into()
        }
        Err(error) => error.into_compile_error(),
    }
}

fn expand(cte: &TableStruct) -> TokenStream {
    let ident = proc_macro2::Ident::new(&cte.ident.to_string(), Span::call_site());
    let cte_name = Literal::string(&to_snake_plural(&ident.to_string()));

    // A zero-sized, object-safe handle to this CTE's definition, held as a `&'static dyn CteDef` by the
    // query's `WITH` collector. Lowering the body needs the canonical metadata type (`<'static,
    // ColumnExpr>`), the same instantiation the `SchemaCte`/`CteDefinition` impls are written against.
    let cte_def_ty = generated_ident(&ident, "", "CteDef");
    let cte_def_static = generated_ident(&ident, "", "CteDefStatic");
    let cte_def_impl = quote::quote! {
        #[doc(hidden)]
        pub struct #cte_def_ty;

        impl ::squealy::CteDef for #cte_def_ty {
            fn name(&self) -> &'static str {
                #cte_name
            }

            fn type_key(&self) -> ::std::any::TypeId {
                ::std::any::TypeId::of::<#cte_def_ty>()
            }

            fn columns(&self) -> ::std::vec::Vec<::squealy::ViewColumnModel> {
                <#ident <'static, ::squealy::ColumnExpr> as ::squealy::SchemaCte>::cte_columns()
            }

            fn body_model(&self) -> ::squealy::ViewQueryModel {
                ::squealy::cte_definition_model::<#ident <'static, ::squealy::ColumnExpr>>()
            }
        }

        #[doc(hidden)]
        static #cte_def_static: #cte_def_ty = #cte_def_ty;

        // A CTE is a `FROM` source that contributes its `WITH` definition when referenced (directly or
        // through a join). This is the only `QuerySource` impl for the type — the table macro skips it
        // in `Cte` mode so there is no conflicting no-CTE impl.
        impl<'scope, C: ::squealy::ColumnMode> ::squealy::QuerySource for #ident <'scope, C>
        where
            #ident <'scope, C>: ::squealy::TableProjection,
        {
            fn cte_def() -> ::std::option::Option<&'static dyn ::squealy::CteDef> {
                ::std::option::Option::Some(&#cte_def_static)
            }
        }
    };

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
        #cte_def_impl

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
