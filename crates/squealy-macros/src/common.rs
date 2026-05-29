use proc_macro::{Group, Literal as ProcLiteral, TokenStream, TokenTree};
use proc_macro2::{Literal, Span};

pub(crate) struct StructField {
    pub ident: proc_macro::Ident,
    pub ty: proc_macro2::TokenStream,
}

pub(crate) fn struct_fields(
    group: &Group,
    missing_type_message: &str,
) -> Result<Vec<StructField>, String> {
    let tokens = group.stream().into_iter().collect::<Vec<_>>();
    let mut fields = Vec::new();
    let mut index = 0;

    while index < tokens.len() {
        if is_attribute_start(&tokens[index]) {
            index += 2;
            continue;
        }

        let TokenTree::Ident(ident) = &tokens[index] else {
            index += 1;
            continue;
        };

        if !matches!(tokens.get(index + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == ':')
        {
            index += 1;
            continue;
        }

        let mut type_tokens = Vec::new();
        let mut angle_depth = 0usize;
        index += 2;

        while index < tokens.len() {
            match &tokens[index] {
                TokenTree::Punct(punct) if punct.as_char() == '<' => {
                    angle_depth += 1;
                    type_tokens.push(tokens[index].clone());
                }
                TokenTree::Punct(punct) if punct.as_char() == '>' => {
                    angle_depth = angle_depth.saturating_sub(1);
                    type_tokens.push(tokens[index].clone());
                }
                TokenTree::Punct(punct) if punct.as_char() == ',' && angle_depth == 0 => {
                    break;
                }
                token => type_tokens.push(token.clone()),
            }
            index += 1;
        }

        if type_tokens.is_empty() {
            return Err(format!(
                "{missing_type_message} `{ident}` is missing a type"
            ));
        }

        fields.push(StructField {
            ident: ident.clone(),
            ty: proc_macro2::TokenStream::from(TokenStream::from_iter(type_tokens)),
        });
        index += 1;
    }

    Ok(fields)
}

pub(crate) fn generated_ident(
    table_ident: &proc_macro2::Ident,
    name: &str,
    suffix: &str,
) -> proc_macro2::Ident {
    proc_macro2::Ident::new(
        &format!("__Squealy{}{}{}", table_ident, to_pascal(name), suffix),
        Span::call_site(),
    )
}

pub(crate) fn foreign_key_ident(column_ident: &proc_macro2::Ident) -> proc_macro2::Ident {
    proc_macro2::Ident::new(&format!("{column_ident}ForeignKey"), Span::call_site())
}

pub(crate) fn to_pascal(name: &str) -> String {
    let mut output = String::new();
    let mut capitalize_next = true;

    for character in name.chars() {
        if character == '_' || !character.is_alphanumeric() {
            capitalize_next = true;
            continue;
        }

        if capitalize_next {
            output.extend(character.to_uppercase());
            capitalize_next = false;
        } else {
            output.push(character);
        }
    }

    if output.is_empty() {
        "Generated".to_owned()
    } else {
        output
    }
}

pub(crate) fn required_literal(name: &str, value_tokens: &[TokenTree]) -> Result<String, String> {
    let Some(token) = value_tokens.first() else {
        return Err(format!("attribute `{name}` requires a string value"));
    };

    Ok(match token {
        TokenTree::Literal(literal) => literal_string(literal),
        token => token.to_string(),
    })
}

pub(crate) fn literal_string(literal: &ProcLiteral) -> String {
    let value = literal.to_string();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&value)
        .to_owned()
}

pub(crate) fn bool_tokens(value: bool) -> proc_macro2::TokenStream {
    if value {
        quote::quote! { true }
    } else {
        quote::quote! { false }
    }
}

pub(crate) fn option_literal(value: Option<&str>) -> proc_macro2::TokenStream {
    match value {
        Some(value) => {
            let value = Literal::string(value);
            quote::quote! { Some(#value) }
        }
        None => quote::quote! { None },
    }
}

pub(crate) fn is_attribute_start(token: &TokenTree) -> bool {
    matches!(token, TokenTree::Punct(punct) if punct.as_char() == '#')
}

pub(crate) fn matches_ident(token: &TokenTree, expected: &str) -> bool {
    matches!(token, TokenTree::Ident(ident) if ident.to_string() == expected)
}

pub(crate) fn to_snake(name: &str) -> String {
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
    out
}

pub(crate) fn to_snake_plural(name: &str) -> String {
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

pub(crate) fn compile_error(message: &str) -> TokenStream {
    let message = Literal::string(message);
    quote::quote! {
        compile_error!(#message);
    }
    .into()
}
