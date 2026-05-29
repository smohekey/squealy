//! Procedural macros for squealy.

use proc_macro::{Delimiter, Group, Ident, Literal as ProcLiteral, TokenStream, TokenTree};
use proc_macro2::{Literal, Span};

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
        column_name
    )
)]
pub fn derive_table(input: TokenStream) -> TokenStream {
    match table_struct(input) {
        Ok(table) => table.expand(),
        Err(message) => compile_error(&message),
    }
}

struct TableStruct {
    ident: Ident,
    fields: Vec<Field>,
    indexes: Vec<IndexAttrs>,
    has_scope_and_mode: bool,
}

struct Field {
    ident: Ident,
    attrs: FieldAttrs,
}

struct IndexAttrs {
    name: Option<String>,
    columns: Vec<Ident>,
    unique: bool,
}

#[derive(Default)]
struct FieldAttrs {
    column_name: Option<String>,
    primary_key: bool,
    index: bool,
    unique: bool,
    nullable: Option<bool>,
    auto_increment: bool,
    default: Option<String>,
    db_type: Option<String>,
    check: Option<String>,
    references: Option<ForeignKeyAttrs>,
}

#[derive(Default)]
struct ForeignKeyAttrs {
    table: Option<String>,
    column: Option<String>,
    on_delete: Option<String>,
    on_update: Option<String>,
}

impl TableStruct {
    fn expand(&self) -> TokenStream {
        if !self.has_scope_and_mode {
            return compile_error(
                "Table currently requires structs shaped like `Type<'scope, C: Column = ColumnExpr>`",
            );
        }

        let ident = proc_macro2::Ident::new(&self.ident.to_string(), Span::call_site());
        let name = Literal::string(&to_snake_plural(&ident.to_string()));
        let fields = self
            .fields
            .iter()
            .map(|field| proc_macro2::Ident::new(&field.ident.to_string(), Span::call_site()))
            .collect::<Vec<_>>();
        let field_literals = self
            .fields
            .iter()
            .map(|field| Literal::string(&field.column_name()))
            .collect::<Vec<_>>();
        let schema_columns = self
            .fields
            .iter()
            .map(Field::schema_column_tokens)
            .collect::<Vec<_>>();
        let mut schema_indexes = self
            .fields
            .iter()
            .filter(|field| field.attrs.index)
            .map(Field::schema_index_tokens)
            .collect::<Vec<_>>();
        schema_indexes.extend(
            self.indexes
                .iter()
                .map(|index| index.schema_index_tokens(&self.fields)),
        );

        quote::quote! {
            impl<'scope, C: ::squealy::Column> ::squealy::Table for #ident <'scope, C> {
                type WithColumn<'next_scope, NextC: ::squealy::Column> = #ident <'next_scope, NextC>
                where
                    NextC: 'next_scope;

                fn name() -> &'static str {
                    #name
                }

                fn schema() -> ::squealy::TableSchema {
                    ::squealy::TableSchema {
                        name: #name,
                        columns: &[#( #schema_columns, )*],
                        indexes: &[#( #schema_indexes, )*],
                    }
                }

                fn column_names() -> Self::WithColumn<'static, ::squealy::ColumnName> {
                    #ident { #( #fields: #field_literals, )* }
                }

                fn columns_from<'next_scope>(
                    alias: &str,
                    columns: &Self::WithColumn<'static, ::squealy::ColumnName>,
                ) -> Self::WithColumn<'next_scope, ::squealy::ColumnExpr> {
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

impl IndexAttrs {
    fn schema_index_tokens(&self, fields: &[Field]) -> proc_macro2::TokenStream {
        let name = option_literal(self.name.as_deref());
        let columns = self
            .columns
            .iter()
            .map(|column| {
                let column = column.to_string();
                let field = fields
                    .iter()
                    .find(|field| field.ident.to_string() == column)
                    .expect("index fields should be validated before code generation");
                Literal::string(&field.column_name())
            })
            .collect::<Vec<_>>();
        let unique = bool_tokens(self.unique);

        quote::quote! {
            ::squealy::IndexSchema {
                name: #name,
                columns: &[#( #columns, )*],
                unique: #unique,
            }
        }
    }
}

impl Field {
    fn column_name(&self) -> String {
        self.attrs
            .column_name
            .clone()
            .unwrap_or_else(|| self.ident.to_string())
    }

    fn schema_column_tokens(&self) -> proc_macro2::TokenStream {
        let name = Literal::string(&self.column_name());
        let primary_key = bool_tokens(self.attrs.primary_key);
        let indexed = bool_tokens(self.attrs.index);
        let unique = bool_tokens(self.attrs.unique);
        let nullable = bool_tokens(self.attrs.nullable.unwrap_or(false));
        let auto_increment = bool_tokens(self.attrs.auto_increment);
        let default = option_literal(self.attrs.default.as_deref());
        let db_type = option_literal(self.attrs.db_type.as_deref());
        let check = option_literal(self.attrs.check.as_deref());
        let references = self.references_tokens();

        quote::quote! {
            ::squealy::ColumnSchema {
                name: #name,
                primary_key: #primary_key,
                indexed: #indexed,
                unique: #unique,
                nullable: #nullable,
                auto_increment: #auto_increment,
                default: #default,
                db_type: #db_type,
                check: #check,
                references: #references,
            }
        }
    }

    fn schema_index_tokens(&self) -> proc_macro2::TokenStream {
        let name = option_literal(None);
        let column_name = Literal::string(&self.column_name());
        let unique = bool_tokens(self.attrs.unique);

        quote::quote! {
            ::squealy::IndexSchema {
                name: #name,
                columns: &[#column_name],
                unique: #unique,
            }
        }
    }

    fn references_tokens(&self) -> proc_macro2::TokenStream {
        let Some(references) = &self.attrs.references else {
            return quote::quote! { None };
        };

        let table = Literal::string(references.table.as_deref().unwrap_or(""));
        let column = Literal::string(references.column.as_deref().unwrap_or("id"));
        let on_delete = option_literal(references.on_delete.as_deref());
        let on_update = option_literal(references.on_update.as_deref());

        quote::quote! {
            Some(::squealy::ForeignKeySchema {
                table: #table,
                column: #column,
                on_delete: #on_delete,
                on_update: #on_update,
            })
        }
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
        .contains("'scope,C:");

    let fields = match &tokens[body_index] {
        TokenTree::Group(group) => named_fields(group)?,
        _ => unreachable!(),
    };
    let indexes = table_indexes(&tokens[..struct_index])?;
    validate_index_columns(&indexes, &fields)?;

    Ok(TableStruct {
        ident,
        fields,
        indexes,
        has_scope_and_mode,
    })
}

fn table_indexes(tokens: &[TokenTree]) -> Result<Vec<IndexAttrs>, String> {
    let mut indexes = Vec::new();
    let mut iter = tokens.iter();

    while let Some(token) = iter.next() {
        if !is_attribute_start(token) {
            continue;
        }

        let Some(TokenTree::Group(attr)) = iter.next() else {
            return Err("Table attribute is missing its bracketed body".to_owned());
        };
        let Some(index) = table_index(attr)? else {
            continue;
        };
        indexes.push(index);
    }

    Ok(indexes)
}

fn table_index(group: &Group) -> Result<Option<IndexAttrs>, String> {
    if group.delimiter() != Delimiter::Bracket {
        return Err("Table attributes must use square brackets".to_owned());
    }

    let mut tokens = group.stream().into_iter();
    let Some(TokenTree::Ident(attr_name)) = tokens.next() else {
        return Ok(None);
    };
    if attr_name.to_string() != "index" {
        return Ok(None);
    }

    let Some(TokenTree::Group(meta)) = tokens.next() else {
        return Err("table-level #[index(...)] requires metadata inside parentheses".to_owned());
    };

    parse_index(meta.stream().into_iter().collect::<Vec<_>>()).map(Some)
}

fn parse_index(tokens: Vec<TokenTree>) -> Result<IndexAttrs, String> {
    let mut index = 0;
    let mut attrs = IndexAttrs {
        name: None,
        columns: Vec::new(),
        unique: false,
    };

    while index < tokens.len() {
        while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
            index += 1;
        }

        let Some(TokenTree::Ident(name)) = tokens.get(index) else {
            break;
        };
        let name = name.to_string();
        index += 1;

        match name.as_str() {
            "unique" => attrs.unique = true,
            "name" => {
                if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=')
                {
                    return Err("index option `name` requires a string value".to_owned());
                }
                index += 1;
                attrs.name = Some(match tokens.get(index) {
                    Some(TokenTree::Literal(literal)) => literal_string(literal),
                    Some(token) => token.to_string(),
                    None => return Err("index option `name` is missing a value".to_owned()),
                });
                index += 1;
            }
            "columns" => {
                if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=')
                {
                    return Err("index option `columns` requires a bracketed field list".to_owned());
                }
                index += 1;
                let Some(TokenTree::Group(columns)) = tokens.get(index) else {
                    return Err("index option `columns` requires a bracketed field list".to_owned());
                };
                if columns.delimiter() != Delimiter::Bracket {
                    return Err("index option `columns` requires square brackets".to_owned());
                }
                attrs.columns = parse_index_columns(columns)?;
                index += 1;
            }
            _ => return Err(format!("unsupported index option `{name}`")),
        }
    }

    if attrs.columns.is_empty() {
        return Err("table-level indexes require at least one column".to_owned());
    }

    Ok(attrs)
}

fn parse_index_columns(group: &Group) -> Result<Vec<Ident>, String> {
    let mut columns = Vec::new();
    for token in group.stream() {
        match token {
            TokenTree::Ident(ident) => columns.push(ident),
            TokenTree::Punct(punct) if punct.as_char() == ',' => {}
            _ => return Err("index columns must be field identifiers".to_owned()),
        }
    }
    Ok(columns)
}

fn validate_index_columns(indexes: &[IndexAttrs], fields: &[Field]) -> Result<(), String> {
    for index in indexes {
        for column in &index.columns {
            let column = column.to_string();
            if !fields.iter().any(|field| field.ident.to_string() == column) {
                return Err(format!("index references unknown field `{column}`"));
            }
        }
    }

    Ok(())
}

fn named_fields(group: &Group) -> Result<Vec<Field>, String> {
    let mut fields = Vec::new();
    let mut pending_attrs = FieldAttrs::default();
    let mut iter = group.stream().into_iter().peekable();

    while let Some(token) = iter.next() {
        if is_attribute_start(&token) {
            let Some(TokenTree::Group(attr)) = iter.next() else {
                return Err("Table field attribute is missing its bracketed body".to_owned());
            };
            apply_attribute(&attr, &mut pending_attrs)?;
            continue;
        }

        let TokenTree::Ident(ident) = token else {
            continue;
        };

        if let Some(TokenTree::Punct(punct)) = iter.peek() {
            if punct.as_char() == ':' && punct.spacing() == proc_macro::Spacing::Alone {
                fields.push(Field {
                    ident,
                    attrs: std::mem::take(&mut pending_attrs),
                });
            }
        }
    }

    if fields.is_empty() {
        Err("Table requires at least one named field".to_owned())
    } else {
        Ok(fields)
    }
}

fn apply_attribute(group: &Group, attrs: &mut FieldAttrs) -> Result<(), String> {
    if group.delimiter() != Delimiter::Bracket {
        return Err("Table field attributes must use square brackets".to_owned());
    }

    let mut tokens = group.stream().into_iter();
    let Some(TokenTree::Ident(attr_name)) = tokens.next() else {
        return Ok(());
    };
    let attr_name = attr_name.to_string();
    let rest = tokens.collect::<Vec<_>>();

    if attr_name == "column" {
        let Some(TokenTree::Group(meta)) = rest.first() else {
            return Err("#[column(...)] requires metadata inside parentheses".to_owned());
        };
        parse_meta_items(meta.stream().into_iter().collect::<Vec<_>>(), attrs)
    } else if matches!(rest.first(), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
        parse_meta_item(&attr_name, &rest[1..], attrs)
    } else if let Some(TokenTree::Group(meta)) = rest.first() {
        parse_meta_item(&attr_name, &[TokenTree::Group(meta.clone())], attrs)
    } else {
        parse_meta_item(&attr_name, &rest, attrs)
    }
}

fn parse_meta_items(tokens: Vec<TokenTree>, attrs: &mut FieldAttrs) -> Result<(), String> {
    let mut index = 0;
    while index < tokens.len() {
        while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
            index += 1;
        }

        let Some(TokenTree::Ident(name)) = tokens.get(index) else {
            break;
        };
        let name = name.to_string();
        index += 1;

        let mut value_tokens = Vec::new();
        if matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
            index += 1;
            while index < tokens.len()
                && !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',')
            {
                value_tokens.push(tokens[index].clone());
                index += 1;
            }
        } else if let Some(TokenTree::Group(group)) = tokens.get(index) {
            value_tokens.push(TokenTree::Group(group.clone()));
            index += 1;
        }

        parse_meta_item(&name, &value_tokens, attrs)?;
    }

    Ok(())
}

fn parse_meta_item(
    name: &str,
    value_tokens: &[TokenTree],
    attrs: &mut FieldAttrs,
) -> Result<(), String> {
    match name {
        "primary_key" => attrs.primary_key = true,
        "index" => attrs.index = true,
        "unique" => attrs.unique = true,
        "nullable" => attrs.nullable = Some(true),
        "not_null" => attrs.nullable = Some(false),
        "auto_increment" => attrs.auto_increment = true,
        "default" => attrs.default = Some(required_literal(name, value_tokens)?),
        "db_type" => attrs.db_type = Some(required_literal(name, value_tokens)?),
        "check" => attrs.check = Some(required_literal(name, value_tokens)?),
        "column_name" | "name" => attrs.column_name = Some(required_literal(name, value_tokens)?),
        "references" => attrs.references = Some(parse_references(value_tokens)?),
        _ => return Err(format!("unsupported Table field attribute `{name}`")),
    }

    Ok(())
}

fn parse_references(value_tokens: &[TokenTree]) -> Result<ForeignKeyAttrs, String> {
    let Some(TokenTree::Group(group)) = value_tokens.first() else {
        return Err(
            "references requires metadata like references(table = \"users\", column = \"id\")"
                .to_owned(),
        );
    };

    let mut references = ForeignKeyAttrs::default();
    let mut index = 0;
    let tokens = group.stream().into_iter().collect::<Vec<_>>();
    while index < tokens.len() {
        while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
            index += 1;
        }

        let Some(TokenTree::Ident(name)) = tokens.get(index) else {
            break;
        };
        let name = name.to_string();
        index += 1;

        if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
            return Err(format!(
                "references option `{name}` requires a string value"
            ));
        }
        index += 1;

        let value = match tokens.get(index) {
            Some(TokenTree::Literal(literal)) => literal_string(literal),
            Some(token) => token.to_string(),
            None => return Err(format!("references option `{name}` is missing a value")),
        };
        index += 1;

        match name.as_str() {
            "table" => references.table = Some(value),
            "column" => references.column = Some(value),
            "on_delete" => references.on_delete = Some(value),
            "on_update" => references.on_update = Some(value),
            _ => return Err(format!("unsupported references option `{name}`")),
        }
    }

    if references.table.is_none() {
        return Err("references requires `table = \"...\"`".to_owned());
    }

    Ok(references)
}

fn required_literal(name: &str, value_tokens: &[TokenTree]) -> Result<String, String> {
    let Some(token) = value_tokens.first() else {
        return Err(format!("attribute `{name}` requires a string value"));
    };

    Ok(match token {
        TokenTree::Literal(literal) => literal_string(literal),
        token => token.to_string(),
    })
}

fn literal_string(literal: &ProcLiteral) -> String {
    let value = literal.to_string();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&value)
        .to_owned()
}

fn bool_tokens(value: bool) -> proc_macro2::TokenStream {
    if value {
        quote::quote! { true }
    } else {
        quote::quote! { false }
    }
}

fn option_literal(value: Option<&str>) -> proc_macro2::TokenStream {
    match value {
        Some(value) => {
            let value = Literal::string(value);
            quote::quote! { Some(#value) }
        }
        None => quote::quote! { None },
    }
}

fn is_attribute_start(token: &TokenTree) -> bool {
    matches!(token, TokenTree::Punct(punct) if punct.as_char() == '#')
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
