use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};
use proc_macro2::{Literal, Span};

use crate::common::{compile_error, matches_ident, required_literal, struct_fields};

pub(crate) fn derive(input: TokenStream) -> TokenStream {
    match column_type_struct(input) {
        Ok(column_type) => column_type.expand(),
        Err(message) => compile_error(&message),
    }
}

struct ColumnTypeStruct {
    ident: Ident,
    field_ty: proc_macro2::TokenStream,
    raw: Option<String>,
}

impl ColumnTypeStruct {
    fn expand(&self) -> TokenStream {
        let ident = proc_macro2::Ident::new(&self.ident.to_string(), Span::call_site());
        let column_type = if let Some(raw) = &self.raw {
            let raw = Literal::string(raw);
            quote::quote! { ::squealy::ColumnType::Raw(#raw) }
        } else {
            let field_ty = &self.field_ty;
            quote::quote! { <#field_ty as ::squealy::HasColumnType>::COLUMN_TYPE }
        };

        quote::quote! {
            impl ::squealy::HasColumnType for #ident {
                const COLUMN_TYPE: ::squealy::ColumnType = #column_type;
            }
        }
        .into()
    }
}

fn column_type_struct(input: TokenStream) -> Result<ColumnTypeStruct, String> {
    let tokens = input.into_iter().collect::<Vec<_>>();
    let raw = column_type_attrs(&tokens)?;
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

    let field_ty = match body.delimiter() {
        Delimiter::Parenthesis => tuple_field_type(&body)?,
        Delimiter::Brace => named_field_type(&body)?,
        _ => unreachable!(),
    };

    Ok(ColumnTypeStruct {
        ident,
        field_ty,
        raw,
    })
}

fn column_type_attrs(tokens: &[TokenTree]) -> Result<Option<String>, String> {
    let mut raw = None;
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

        raw = Some(column_type_attr_raw(&meta)?);
        index += 2;
    }

    Ok(raw)
}

fn column_type_attr_raw(group: &Group) -> Result<String, String> {
    let tokens = group.stream().into_iter().collect::<Vec<_>>();
    let Some(TokenTree::Ident(name)) = tokens.first() else {
        return Err("unsupported ColumnType attribute".to_owned());
    };

    if name.to_string() != "raw" {
        return Err(format!("unsupported ColumnType attribute `{name}`"));
    }

    if !matches!(tokens.get(1), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
        return Err("attribute `raw` requires a string value".to_owned());
    }

    required_literal("raw", &tokens[2..])
}

fn named_field_type(group: &Group) -> Result<proc_macro2::TokenStream, String> {
    let fields = struct_fields(group, "ColumnType field")?;

    if fields.len() != 1 {
        return Err("ColumnType requires exactly one field".to_owned());
    }

    Ok(fields.into_iter().next().unwrap().ty)
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
