//! Procedural macros for squealy.

use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};
use proc_macro2::{Literal, Span};

/// Derives squealy table metadata for generic table-mode structs.
#[proc_macro_derive(Table)]
pub fn derive_table(input: TokenStream) -> TokenStream {
    match table_struct(input) {
        Ok(table) => table.expand(),
        Err(message) => compile_error(&message),
    }
}

struct TableStruct {
    ident: Ident,
    fields: Vec<Ident>,
    has_scope_and_mode: bool,
}

impl TableStruct {
    fn expand(&self) -> TokenStream {
        if !self.has_scope_and_mode {
            return compile_error(
                "Table currently requires structs shaped like `Type<'scope, Mode: Column = ColumnExpr>`",
            );
        }

        let ident = proc_macro2::Ident::new(&self.ident.to_string(), Span::call_site());
        let name = Literal::string(&to_snake_plural(&ident.to_string()));
        let field_names = self
            .fields
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let fields = field_names
            .iter()
            .map(|field| proc_macro2::Ident::new(field, Span::call_site()))
            .collect::<Vec<_>>();
        let field_literals = field_names
            .iter()
            .map(|field| Literal::string(field))
            .collect::<Vec<_>>();

        quote::quote! {
            impl<'scope, Mode: ::squealy::Column> ::squealy::Table for #ident <'scope, Mode> {
                type WithMode<'next_scope, NextMode: ::squealy::Column> = #ident <'next_scope, NextMode>
                where
                    NextMode: 'next_scope;

                fn name() -> &'static str {
                    #name
                }

                fn column_names() -> Self::WithMode<'static, ::squealy::ColumnName> {
                    #ident { #( #fields: #field_literals, )* }
                }

                fn columns_from<'next_scope>(
                    alias: &str,
                    columns: &Self::WithMode<'static, ::squealy::ColumnName>,
                ) -> Self::WithMode<'next_scope, ::squealy::ColumnExpr> {
                    #ident { #( #fields: ::squealy::Expr::column(alias, columns.#fields), )* }
                }
            }

            impl<'scope> ::squealy::Projectable for #ident <'scope, ::squealy::ColumnExpr> {
                fn project(&self) -> ::std::vec::Vec<::squealy::SelectColumn> {
                    ::std::vec![#( ::squealy::SelectColumn::new(self.#fields.to_sql().to_owned(), #field_literals), )*]
                }

                fn re_alias(&self, alias: &str) -> Self {
                    #ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }
            }
        }
        .into()
    }
}

fn table_struct(input: TokenStream) -> Result<TableStruct, String> {
    let tokens = input.into_iter().collect::<Vec<_>>();
    let struct_index = tokens
        .iter()
        .position(|token| matches_ident(token, "struct"))
        .ok_or_else(|| "Table can only be derived for structs".to_owned())?;

    let ident = tokens
        .get(struct_index + 1)
        .and_then(|token| match token {
            TokenTree::Ident(ident) => Some(ident.clone()),
            _ => None,
        })
        .ok_or_else(|| "Table derive could not find the struct name".to_owned())?;

    let body_index = tokens
        .iter()
        .position(|token| matches!(token, TokenTree::Group(group) if group.delimiter() == Delimiter::Brace))
        .ok_or_else(|| "Table requires a named-field struct".to_owned())?;

    let has_scope_and_mode = tokens[struct_index + 2..body_index]
        .iter()
        .map(ToString::to_string)
        .collect::<String>()
        .contains("'scope,Mode:");

    let fields = match &tokens[body_index] {
        TokenTree::Group(group) => named_fields(group)?,
        _ => unreachable!(),
    };

    Ok(TableStruct {
        ident,
        fields,
        has_scope_and_mode,
    })
}

fn named_fields(group: &Group) -> Result<Vec<Ident>, String> {
    let mut fields = Vec::new();
    let mut iter = group.stream().into_iter().peekable();

    while let Some(token) = iter.next() {
        let TokenTree::Ident(ident) = token else {
            continue;
        };

        if let Some(TokenTree::Punct(punct)) = iter.peek() {
            if punct.as_char() == ':' && punct.spacing() == proc_macro::Spacing::Alone {
                fields.push(ident);
            }
        }
    }

    if fields.is_empty() {
        Err("Table requires at least one named field".to_owned())
    } else {
        Ok(fields)
    }
}

fn matches_ident(token: &TokenTree, expected: &str) -> bool {
    matches!(token, TokenTree::Ident(ident) if ident.to_string() == expected)
}

fn to_snake_plural(name: &str) -> String {
    let mut out = String::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out.push('s');
    out
}

fn compile_error(message: &str) -> TokenStream {
    let message = Literal::string(message);
    quote::quote! {
        compile_error!(#message);
    }
    .into()
}
