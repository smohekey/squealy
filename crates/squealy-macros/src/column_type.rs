use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};
use proc_macro2::Span;

use crate::common::{compile_error, matches_ident, required_literal, struct_fields};

pub(crate) fn derive(input: TokenStream) -> TokenStream {
    match column_type_struct(input) {
        Ok(column_type) => column_type.expand(),
        Err(message) => compile_error(&message),
    }
}

struct ColumnTypeStruct {
    ident: Ident,
    field: ColumnTypeField,
    db_type: Option<String>,
}

enum ColumnTypeField {
    Tuple(proc_macro2::TokenStream),
    Named {
        ident: Ident,
        ty: proc_macro2::TokenStream,
    },
}

impl ColumnTypeStruct {
    fn expand(&self) -> TokenStream {
        let ident = proc_macro2::Ident::new(&self.ident.to_string(), Span::call_site());
        let field_ty = self.field.ty();
        let column_type = if let Some(db_type) = &self.db_type {
            crate::common::parse_db_type(db_type)
        } else {
            quote::quote! { <#field_ty as ::squealy::HasColumnType>::COLUMN_TYPE }
        };
        let deconstruct = self.field.deconstruct();
        let construct = self.field.construct();

        quote::quote! {
            impl ::squealy::HasColumnType for #ident {
                const COLUMN_TYPE: ::squealy::ColumnType = #column_type;
            }

            impl ::squealy::ExprKind for #ident {
                type Value = Self;
            }

            impl ::squealy::IntoBindValue for #ident {
                fn into_bind_value(self) -> ::squealy::BindValue {
                    let #deconstruct = self;
                    ::squealy::IntoBindValue::into_bind_value(value)
                }
            }

            impl<Backend> ::squealy::Decode<Backend> for #ident
            where
                Backend: ::squealy::Backend,
                #field_ty: ::squealy::Decode<Backend>,
            {
                fn decode(
                    row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
                ) -> ::std::result::Result<Self, <Backend as ::squealy::Backend>::Error> {
                    let value = ::squealy::RowReader::read::<#field_ty>(row)?;
                    ::std::result::Result::Ok(#construct)
                }
            }

            impl<Backend> ::squealy::DecodeNullable<Backend> for #ident
            where
                Backend: ::squealy::Backend,
                #field_ty: ::squealy::DecodeNullable<Backend>,
            {
                fn decode_nullable(
                    row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
                ) -> ::std::result::Result<::std::option::Option<Self>, <Backend as ::squealy::Backend>::Error> {
                    let value = <#field_ty as ::squealy::DecodeNullable<Backend>>::decode_nullable(row)?;
                    ::std::result::Result::Ok(value.map(|value| #construct))
                }
            }
        }
        .into()
    }
}

impl ColumnTypeField {
    fn ty(&self) -> &proc_macro2::TokenStream {
        match self {
            Self::Tuple(ty) | Self::Named { ty, .. } => ty,
        }
    }

    fn deconstruct(&self) -> proc_macro2::TokenStream {
        match self {
            Self::Tuple(_) => quote::quote! { Self(value) },
            Self::Named { ident, .. } => {
                let ident = proc_macro2::Ident::new(&ident.to_string(), Span::call_site());
                quote::quote! { Self { #ident: value } }
            }
        }
    }

    fn construct(&self) -> proc_macro2::TokenStream {
        match self {
            Self::Tuple(_) => quote::quote! { Self(value) },
            Self::Named { ident, .. } => {
                let ident = proc_macro2::Ident::new(&ident.to_string(), Span::call_site());
                quote::quote! { Self { #ident: value } }
            }
        }
    }
}

fn column_type_struct(input: TokenStream) -> Result<ColumnTypeStruct, String> {
    let tokens = input.into_iter().collect::<Vec<_>>();
    let db_type = column_type_attrs(&tokens)?;
    let struct_index = tokens
        .iter()
        .position(|token| matches_ident(token, "struct"))
        .ok_or_else(|| "ColumnType can only be derived for structs".to_owned())?;

    let ident = tokens
        .get(struct_index + 1)
        .and_then(|token| match token {
            TokenTree::Ident(ident) => Some(ident.clone()),
            _ => None,
        })
        .ok_or_else(|| "ColumnType derive could not find the struct name".to_owned())?;

    let body = tokens
        .iter()
        .skip(struct_index + 2)
        .find_map(|token| match token {
            TokenTree::Group(group)
                if matches!(group.delimiter(), Delimiter::Brace | Delimiter::Parenthesis) =>
            {
                Some(group.clone())
            }
            _ => None,
        })
        .ok_or_else(|| "ColumnType requires a single-field struct".to_owned())?;

    let field = match body.delimiter() {
        Delimiter::Parenthesis => ColumnTypeField::Tuple(tuple_field_type(&body)?),
        Delimiter::Brace => named_field(&body)?,
        _ => unreachable!(),
    };

    Ok(ColumnTypeStruct {
        ident,
        field,
        db_type,
    })
}

fn column_type_attrs(tokens: &[TokenTree]) -> Result<Option<String>, String> {
    let mut db_type = None;
    let mut index = 0;

    while index < tokens.len() {
        if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '#') {
            index += 1;
            continue;
        }

        let Some(TokenTree::Group(attr)) = tokens.get(index + 1) else {
            index += 1;
            continue;
        };

        let mut attr_tokens = attr.stream().into_iter();
        let Some(TokenTree::Ident(name)) = attr_tokens.next() else {
            index += 2;
            continue;
        };

        if name.to_string() != "column_type" {
            index += 2;
            continue;
        }

        let Some(TokenTree::Group(meta)) = attr_tokens.next() else {
            return Err("#[column_type(...)] requires metadata inside parentheses".to_owned());
        };

        if meta.delimiter() != Delimiter::Parenthesis {
            return Err("#[column_type(...)] requires metadata inside parentheses".to_owned());
        }

        db_type = Some(column_type_attr_db_type(&meta)?);
        index += 2;
    }

    Ok(db_type)
}

fn column_type_attr_db_type(group: &Group) -> Result<String, String> {
    let tokens = group.stream().into_iter().collect::<Vec<_>>();
    let Some(TokenTree::Ident(name)) = tokens.first() else {
        return Err("unsupported ColumnType attribute".to_owned());
    };

    if name.to_string() != "db_type" {
        return Err(format!("unsupported ColumnType attribute `{name}`"));
    }

    if !matches!(tokens.get(1), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
        return Err("attribute `db_type` requires a string value".to_owned());
    }

    required_literal("db_type", &tokens[2..])
}

fn named_field(group: &Group) -> Result<ColumnTypeField, String> {
    let fields = struct_fields(group, "ColumnType field")?;

    if fields.len() != 1 {
        return Err("ColumnType requires exactly one field".to_owned());
    }

    let field = fields.into_iter().next().unwrap();
    Ok(ColumnTypeField::Named {
        ident: field.ident,
        ty: field.ty,
    })
}

fn tuple_field_type(group: &Group) -> Result<proc_macro2::TokenStream, String> {
    let fields = tuple_field_types(group)?;

    if fields.len() != 1 {
        return Err("ColumnType requires exactly one field".to_owned());
    }

    Ok(fields.into_iter().next().unwrap())
}

fn tuple_field_types(group: &Group) -> Result<Vec<proc_macro2::TokenStream>, String> {
    let tokens = group.stream().into_iter().collect::<Vec<_>>();
    let mut fields = Vec::new();
    let mut field_tokens = Vec::new();
    let mut angle_depth = 0usize;

    for token in tokens {
        match &token {
            TokenTree::Punct(punct) if punct.as_char() == '<' => {
                angle_depth += 1;
                field_tokens.push(token);
            }
            TokenTree::Punct(punct) if punct.as_char() == '>' => {
                angle_depth = angle_depth.saturating_sub(1);
                field_tokens.push(token);
            }
            TokenTree::Punct(punct) if punct.as_char() == ',' && angle_depth == 0 => {
                push_tuple_field(&mut fields, std::mem::take(&mut field_tokens))?;
            }
            token => field_tokens.push(token.clone()),
        }
    }

    push_tuple_field(&mut fields, field_tokens)?;
    Ok(fields)
}

fn push_tuple_field(
    fields: &mut Vec<proc_macro2::TokenStream>,
    mut tokens: Vec<TokenTree>,
) -> Result<(), String> {
    trim_tuple_field(&mut tokens);
    if tokens.is_empty() {
        return Ok(());
    }

    fields.push(proc_macro2::TokenStream::from(TokenStream::from_iter(
        tokens,
    )));
    Ok(())
}

fn trim_tuple_field(tokens: &mut Vec<TokenTree>) {
    while matches!(tokens.first(), Some(TokenTree::Punct(punct)) if punct.as_char() == '#') {
        tokens.drain(..2);
    }

    if matches!(tokens.first(), Some(TokenTree::Ident(ident)) if ident.to_string() == "pub") {
        tokens.remove(0);
        if matches!(tokens.first(), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis)
        {
            tokens.remove(0);
        }
    }
}
