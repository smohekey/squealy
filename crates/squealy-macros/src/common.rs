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
    let repr = literal.to_string();

    // Raw strings (`r"..."`, `r#"..."#`) carry no escape sequences.
    if let Some(rest) = repr.strip_prefix('r') {
        let hashes = rest.chars().take_while(|&ch| ch == '#').count();
        if let Some(body) = rest[hashes..].strip_prefix('"') {
            let end = body.len().saturating_sub(1 + hashes);
            return body[..end].to_owned();
        }
        return repr;
    }

    // Regular string literal: strip the quotes and decode escape sequences so
    // values like `"a\"b"` or `"line\n"` reach the generated code unescaped.
    match repr
        .strip_prefix('"')
        .and_then(|body| body.strip_suffix('"'))
    {
        Some(body) => unescape_string(body),
        None => repr,
    }
}

fn unescape_string(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some('x') => {
                let hex = chars.by_ref().take(2).collect::<String>();
                if let Ok(code) = u8::from_str_radix(&hex, 16) {
                    out.push(code as char);
                }
            }
            Some('u') => {
                if chars.next() == Some('{') {
                    let hex = chars
                        .by_ref()
                        .take_while(|&ch| ch != '}')
                        .collect::<String>();
                    if let Some(decoded) =
                        u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                    {
                        out.push(decoded);
                    }
                }
            }
            // Unknown escape: preserve it verbatim rather than silently dropping.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    out
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
    let chars = name.chars().collect::<Vec<_>>();
    let mut out = String::new();

    for (index, &ch) in chars.iter().enumerate() {
        if ch.is_uppercase() {
            // Insert a boundary underscore when transitioning into an uppercase
            // letter from a lowercase/digit, or when an acronym run ends just
            // before a lowercase letter. This keeps `UserName` -> `user_name`
            // and `HTTPServer` -> `http_server` rather than `h_t_t_p_server`.
            let prev_is_lower_or_digit =
                index > 0 && (chars[index - 1].is_lowercase() || chars[index - 1].is_ascii_digit());
            let next_is_lower = chars.get(index + 1).is_some_and(|next| next.is_lowercase());
            if index > 0 && (prev_is_lower_or_digit || next_is_lower) {
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
    let mut out = to_snake(name);
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

/// A macro error carrying a diagnostic message and the source span it refers to.
///
/// Leaf parsing helpers can return a plain `String` (or `&str`); the `From`
/// conversions attach [`Span::call_site`] so existing `?` propagation keeps
/// working. Use [`MacroError::spanned`] to point a diagnostic at the offending
/// token instead of the whole derive input.
pub(crate) struct MacroError {
    message: String,
    span: Span,
}

impl MacroError {
    pub(crate) fn spanned(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }

    pub(crate) fn into_compile_error(self) -> TokenStream {
        let message = Literal::string(&self.message);
        let span = self.span;
        quote::quote_spanned! { span => compile_error!(#message); }.into()
    }
}

impl From<String> for MacroError {
    fn from(message: String) -> Self {
        Self {
            message,
            span: Span::call_site(),
        }
    }
}

impl From<&str> for MacroError {
    fn from(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            span: Span::call_site(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_pascal_capitalizes_each_word() {
        assert_eq!(to_pascal("user"), "User");
        assert_eq!(to_pascal("user_name"), "UserName");
        assert_eq!(to_pascal("current_timestamp"), "CurrentTimestamp");
    }

    #[test]
    fn to_pascal_falls_back_to_generated_when_empty() {
        assert_eq!(to_pascal(""), "Generated");
        assert_eq!(to_pascal("_"), "Generated");
    }

    #[test]
    fn to_snake_inserts_underscores_before_uppercase() {
        assert_eq!(to_snake("User"), "user");
        assert_eq!(to_snake("UserName"), "user_name");
        assert_eq!(to_snake("HTTPServer"), "http_server");
        assert_eq!(to_snake("ApiV2Client"), "api_v2_client");
    }

    #[test]
    fn to_snake_plural_appends_trailing_s() {
        assert_eq!(to_snake_plural("User"), "users");
        assert_eq!(to_snake_plural("UserName"), "user_names");
    }

    #[test]
    fn bool_tokens_render_literals() {
        assert_eq!(bool_tokens(true).to_string(), "true");
        assert_eq!(bool_tokens(false).to_string(), "false");
    }

    #[test]
    fn unescape_string_decodes_common_escapes() {
        assert_eq!(unescape_string("plain"), "plain");
        assert_eq!(unescape_string(r#"a\"b"#), "a\"b");
        assert_eq!(unescape_string(r"back\\slash"), "back\\slash");
        assert_eq!(unescape_string(r"line\nbreak"), "line\nbreak");
        assert_eq!(unescape_string(r"tab\there"), "tab\there");
        assert_eq!(unescape_string(r"hex\x41"), "hexA");
        assert_eq!(unescape_string(r"uni\u{2603}"), "uni\u{2603}");
    }

    #[test]
    fn unescape_string_preserves_unknown_escapes() {
        assert_eq!(unescape_string(r"keep\qhere"), r"keep\qhere");
    }

    #[test]
    fn option_literal_renders_some_and_none() {
        assert_eq!(option_literal(None).to_string(), "None");

        let some = option_literal(Some("anonymous")).to_string();
        assert!(some.starts_with("Some"), "unexpected tokens: {some}");
        assert!(some.contains("\"anonymous\""), "unexpected tokens: {some}");
    }
}
