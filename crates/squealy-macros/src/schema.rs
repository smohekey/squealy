use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};
use proc_macro2::{Literal, Span};

use crate::common::{compile_error, generated_ident, matches_ident, struct_fields, to_snake};

pub(crate) fn derive(input: TokenStream) -> TokenStream {
    match schema_struct(input) {
        Ok(schema) => schema.expand(),
        Err(message) => compile_error(&message),
    }
}

struct SchemaStruct {
    ident: Ident,
    fields: Vec<SchemaField>,
}

struct SchemaField {
    ident: Ident,
    ty: proc_macro2::TokenStream,
}

impl SchemaStruct {
    fn expand(&self) -> TokenStream {
        let ident = proc_macro2::Ident::new(&self.ident.to_string(), Span::call_site());
        let name = Literal::string(&to_snake(&ident.to_string()));
        let table_idents = self
            .fields
            .iter()
            .map(|field| generated_ident(&ident, &field.ident.to_string(), "Table"))
            .collect::<Vec<_>>();
        let table_types = self
            .fields
            .iter()
            .map(|field| &field.ty)
            .collect::<Vec<_>>();
        let table_defs = table_idents
            .iter()
            .zip(table_types.iter())
            .map(|(table_ident, table_type)| {
                quote::quote! {
                    struct #table_ident;

                    impl ::squealy::Table for #table_ident {
                        fn schema_name(&self) -> Option<&'static str> {
                            <#table_type as ::squealy::SchemaTable>::schema_name()
                        }

                        fn name(&self) -> &'static str {
                            <#table_type as ::squealy::SchemaTable>::name()
                        }

                        fn columns(&self) -> &'static [&'static dyn ::squealy::Column] {
                            <#table_type as ::squealy::SchemaTable>::columns()
                        }

                        fn indexes(&self) -> &'static [&'static dyn ::squealy::Index] {
                            <#table_type as ::squealy::SchemaTable>::indexes()
                        }

                        fn primary_key(&self) -> Option<::squealy::TablePrimaryKey> {
                            <#table_type as ::squealy::SchemaTable>::primary_key()
                        }
                    }
                }
            })
            .collect::<Vec<_>>();
        let tables_static = generated_ident(&ident, "tables", "Static");
        let tables_len = Literal::usize_unsuffixed(table_idents.len());

        quote::quote! {
            #(#table_defs)*

            static #tables_static: [&'static (dyn ::squealy::Table + Sync); #tables_len] = [#( &#table_idents, )*];

            impl ::squealy::Schema for #ident {
                fn name() -> Option<&'static str> {
                    Some(#name)
                }

                fn tables() -> impl Iterator<Item = &'static (dyn ::squealy::Table + Sync)> {
                    #tables_static.into_iter()
                }
            }
        }
        .into()
    }
}

fn schema_struct(input: TokenStream) -> Result<SchemaStruct, String> {
    let tokens = input.into_iter().collect::<Vec<_>>();
    let struct_index = tokens
        .iter()
        .position(|token| matches_ident(token, "struct"))
        .ok_or_else(|| "Schema can only be derived for structs".to_owned())?;

    let ident = tokens
        .get(struct_index + 1)
        .and_then(|token| match token {
            TokenTree::Ident(ident) => Some(ident.clone()),
            _ => None,
        })
        .ok_or_else(|| "Schema derive could not find the struct name".to_owned())?;

    let body_index = tokens
        .iter()
        .position(|token| matches!(token, TokenTree::Group(group) if group.delimiter() == Delimiter::Brace))
        .ok_or_else(|| "Schema requires a named-field struct".to_owned())?;

    let fields = match &tokens[body_index] {
        TokenTree::Group(group) => schema_fields(group)?,
        _ => unreachable!(),
    };

    Ok(SchemaStruct { ident, fields })
}

fn schema_fields(group: &Group) -> Result<Vec<SchemaField>, String> {
    struct_fields(group, "schema field").map(|fields| {
        fields
            .into_iter()
            .map(|field| SchemaField {
                ident: field.ident,
                ty: field.ty,
            })
            .collect()
    })
}
