use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};
use proc_macro2::{Literal, Span, TokenStream as TokenStream2};

use crate::common::{
    MacroError, bool_tokens, foreign_key_ident, generated_ident, is_attribute_start,
    literal_string, matches_ident, option_literal, parse_db_type, required_literal, to_pascal,
    to_snake_plural,
};

pub(crate) fn derive(input: TokenStream) -> TokenStream {
    match table_struct(input) {
        Ok(table) => table.expand(),
        Err(error) => error.into_compile_error(),
    }
}

struct TableStruct {
    ident: Ident,
    visibility: TokenStream2,
    fields: Vec<Field>,
    indexes: Vec<IndexAttrs>,
    primary_key: Option<PrimaryKeyAttrs>,
    uniques: Vec<UniqueAttrs>,
    schema: Option<proc_macro2::TokenStream>,
}

struct Field {
    ident: Ident,
    value_ty: proc_macro2::TokenStream,
    attrs: FieldAttrs,
}

struct IndexAttrs {
    name: Option<String>,
    columns: Vec<Ident>,
    unique: bool,
}

struct PrimaryKeyAttrs {
    name: Option<String>,
    columns: Vec<Ident>,
}

struct UniqueAttrs {
    name: Option<String>,
    columns: Vec<Ident>,
}

#[derive(Default)]
struct TableAttrs {
    indexes: Vec<IndexAttrs>,
    primary_key: Option<PrimaryKeyAttrs>,
    uniques: Vec<UniqueAttrs>,
    schema: Option<proc_macro2::TokenStream>,
}

#[derive(Default)]
struct FieldAttrs {
    column_name: Option<String>,
    primary_key: bool,
    index: bool,
    unique: bool,
    nullable: Option<bool>,
    auto_increment: bool,
    generated: bool,
    insert: Option<bool>,
    update: Option<bool>,
    default: Option<DefaultAttrs>,
    db_type: Option<String>,
    check: Option<String>,
    references: Option<ForeignKeyAttrs>,
}

enum DefaultAttrs {
    Literal(DefaultLiteral),
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
    Raw(String),
}

enum DefaultLiteral {
    Null,
    Int(i128),
    UInt(u128),
    Float(f64),
    Text(String),
    Bool(bool),
}

#[derive(Default)]
struct ForeignKeyAttrs {
    table: Option<Ident>,
    column: Option<Ident>,
    on_delete: Option<String>,
    on_update: Option<String>,
}

impl TableStruct {
    fn expand(&self) -> TokenStream {
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
        let column_idents = self
            .fields
            .iter()
            .map(|field| generated_ident(&ident, &field.ident.to_string(), "Column"))
            .collect::<Vec<_>>();
        let exprs_ident = generated_ident(&ident, "exprs", "Projection");
        let rebound_exprs_ident = generated_ident(&ident, "exprs", "ReboundProjection");
        let nullable_exprs_ident = generated_ident(&ident, "nullable_exprs", "Projection");
        let nullable_rebound_exprs_ident =
            generated_ident(&ident, "nullable_exprs", "ReboundProjection");
        let row_shape_ident = generated_ident(&ident, "row", "Shape");
        let expr_kind_idents = self
            .fields
            .iter()
            .map(|field| {
                proc_macro2::Ident::new(
                    &format!("{}{}", ident, to_pascal(&field.ident.to_string())),
                    Span::call_site(),
                )
            })
            .collect::<Vec<_>>();
        let field_value_tys = self
            .fields
            .iter()
            .map(|field| field.value_ty.clone())
            .collect::<Vec<_>>();
        // The type a single-column projection decodes to. A `#[column(nullable)]` column projects as
        // `Option<T>` (so a NULL decodes instead of erroring), matching how the whole-row decode
        // already treats nullable fields; non-null columns project as the bare value type. Predicate
        // operands still use the bare `ExprKind::Value` (`T`), so this does not affect `where_`.
        let projection_row_value_tys = self
            .fields
            .iter()
            .map(|field| {
                let value_ty = field.value_ty.clone();
                if field.nullable() {
                    quote::quote! { ::std::option::Option<#value_ty> }
                } else {
                    quote::quote! { #value_ty }
                }
            })
            .collect::<Vec<_>>();
        // For each `references(Table::column)` foreign key, assert at compile time that the local
        // column's value type matches the referenced column's — so a mismatched FK fails to compile
        // rather than producing DDL the database rejects.
        let fk_type_assertions = self
            .fields
            .iter()
            .filter_map(|field| {
                let references = field.attrs.references.as_ref()?;
                let table = references.table.as_ref()?;
                let column = references.column.as_ref()?;
                let referenced_marker = proc_macro2::Ident::new(
                    &format!("{}{}", table, to_pascal(&column.to_string())),
                    Span::call_site(),
                );
                let local_value_ty = field.value_ty.clone();
                Some(quote::quote! {
                    const _: fn() = || {
                        fn assert_foreign_key_column_type<A, B>()
                        where
                            A: ::squealy::SameValue<B>,
                        {
                        }
                        assert_foreign_key_column_type::<
                            #local_value_ty,
                            <#referenced_marker as ::squealy::ExprKind>::Value,
                        >();
                    };
                })
            })
            .collect::<Vec<_>>();
        let row_field_value_tys = self
            .fields
            .iter()
            .map(|field| {
                let field_ty = field.value_ty.clone();
                if field.nullable() {
                    quote::quote! { ::std::option::Option<#field_ty> }
                } else {
                    field_ty
                }
            })
            .collect::<Vec<_>>();
        let row_field_decode_bounds = self
            .fields
            .iter()
            .map(|field| {
                let field_ty = field.value_ty.clone();
                if field.nullable() {
                    quote::quote! { #field_ty: ::squealy::DecodeNullable<Backend> }
                } else {
                    quote::quote! { #field_ty: ::squealy::Decode<Backend> }
                }
            })
            .collect::<Vec<_>>();
        let row_field_decode_values = self
            .fields
            .iter()
            .map(|field| {
                let field_ty = field.value_ty.clone();
                if field.nullable() {
                    quote::quote! { <#field_ty as ::squealy::DecodeNullable<Backend>>::decode_nullable(row)? }
                } else {
                    quote::quote! { ::squealy::RowReader::read::<#field_ty>(row)? }
                }
            })
            .collect::<Vec<_>>();
        let field_indexes = self
            .fields
            .iter()
            .filter(|field| field.attrs.index)
            .map(|field| {
                (
                    field,
                    generated_ident(&ident, &field.ident.to_string(), "Index"),
                )
            })
            .collect::<Vec<_>>();
        let mut index_idents = field_indexes
            .iter()
            .map(|(_, ident)| ident.clone())
            .collect::<Vec<_>>();
        index_idents.extend(
            self.indexes
                .iter()
                .enumerate()
                .map(|(index, _)| generated_ident(&ident, &index.to_string(), "Index")),
        );
        let column_defs = self
            .fields
            .iter()
            .zip(column_idents.iter())
            .map(|(field, ident)| field.column_definition_tokens(ident))
            .collect::<Vec<_>>();
        let foreign_key_defs = self
            .fields
            .iter()
            .zip(column_idents.iter())
            .filter_map(|(field, ident)| field.foreign_key_definition_tokens(ident))
            .collect::<Vec<_>>();
        let mut index_defs = field_indexes
            .iter()
            .map(|(field, ident)| field.index_definition_tokens(ident))
            .collect::<Vec<_>>();
        let field_index_count = index_defs.len();
        index_defs.extend(self.indexes.iter().enumerate().map(|(index, attrs)| {
            attrs.index_definition_tokens(&index_idents[field_index_count + index], &self.fields)
        }));
        let columns_static = generated_ident(&ident, "columns", "Static");
        let indexes_static = generated_ident(&ident, "indexes", "Static");
        let columns_len = Literal::usize_unsuffixed(column_idents.len());
        let indexes_len = Literal::usize_unsuffixed(index_idents.len());
        // When a table-level `#[primary_key(...)]` is declared, emit a static column-name list and
        // override `primary_key()` on both trait impls. Otherwise emit nothing and let the trait
        // defaults return `None`, leaving the per-column hoist in the model builder untouched.
        let primary_key_static = generated_ident(&ident, "primary_key", "Static");
        let (primary_key_static_def, table_primary_key_method, schema_primary_key_method) =
            match self.primary_key.as_ref() {
                Some(primary_key) => {
                    let name = option_literal(primary_key.name.as_deref());
                    let pk_columns = primary_key
                        .columns
                        .iter()
                        .map(|column| {
                            let column = column.to_string();
                            let field = self
                                .fields
                                .iter()
                                .find(|field| field.ident.to_string() == column)
                                .expect("primary key fields are validated before code generation");
                            Literal::string(&field.column_name())
                        })
                        .collect::<Vec<_>>();
                    let pk_len = Literal::usize_unsuffixed(pk_columns.len());
                    (
                        quote::quote! {
                            static #primary_key_static: [&'static str; #pk_len] = [#( #pk_columns, )*];
                        },
                        quote::quote! {
                            fn primary_key(&self) -> Option<::squealy::TablePrimaryKey> {
                                <Self as ::squealy::SchemaTable>::primary_key()
                            }
                        },
                        quote::quote! {
                            fn primary_key() -> Option<::squealy::TablePrimaryKey> {
                                Some(::squealy::TablePrimaryKey {
                                    name: #name,
                                    columns: &#primary_key_static,
                                })
                            }
                        },
                    )
                }
                None => (quote::quote! {}, quote::quote! {}, quote::quote! {}),
            };
        // Table-level `#[unique(columns = [..])]` constraints. Each one gets a static column-name
        // list; the constraints themselves are gathered into a single static slice and surfaced
        // through `uniques()` on both trait impls. Tables without any declaration fall through to
        // the trait defaults (an empty slice).
        let unique_column_statics = self
            .uniques
            .iter()
            .enumerate()
            .map(|(unique, attrs)| {
                let columns_static =
                    generated_ident(&ident, &format!("unique_{unique}"), "ColumnsStatic");
                let columns = attrs
                    .columns
                    .iter()
                    .map(|column| {
                        let column = column.to_string();
                        let field = self
                            .fields
                            .iter()
                            .find(|field| field.ident.to_string() == column)
                            .expect("unique fields are validated before code generation");
                        Literal::string(&field.column_name())
                    })
                    .collect::<Vec<_>>();
                let columns_len = Literal::usize_unsuffixed(columns.len());
                quote::quote! {
                    static #columns_static: [&'static str; #columns_len] = [#( #columns, )*];
                }
            })
            .collect::<Vec<_>>();
        let unique_entries = self
            .uniques
            .iter()
            .enumerate()
            .map(|(unique, attrs)| {
                let columns_static =
                    generated_ident(&ident, &format!("unique_{unique}"), "ColumnsStatic");
                let name = option_literal(attrs.name.as_deref());
                quote::quote! {
                    ::squealy::TableUnique {
                        name: #name,
                        columns: &#columns_static,
                    }
                }
            })
            .collect::<Vec<_>>();
        let uniques_static = generated_ident(&ident, "uniques", "Static");
        let uniques_len = Literal::usize_unsuffixed(unique_entries.len());
        let (uniques_static_def, table_uniques_method, schema_uniques_method) =
            if self.uniques.is_empty() {
                (quote::quote! {}, quote::quote! {}, quote::quote! {})
            } else {
                (
                    quote::quote! {
                        #( #unique_column_statics )*
                        static #uniques_static: [::squealy::TableUnique; #uniques_len] =
                            [#( #unique_entries, )*];
                    },
                    quote::quote! {
                        fn uniques(&self) -> &'static [::squealy::TableUnique] {
                            <Self as ::squealy::SchemaTable>::uniques()
                        }
                    },
                    quote::quote! {
                        fn uniques() -> &'static [::squealy::TableUnique] {
                            &#uniques_static
                        }
                    },
                )
            };
        let schema = self
            .schema
            .clone()
            .unwrap_or_else(|| quote::quote! { ::squealy::DefaultSchema });
        let write_builder = self.write_builder_tokens(&ident, &expr_kind_idents);
        let visibility = &self.visibility;
        let write_builder_defs = write_builder.definitions;
        let write_table_impl = write_builder.table_impl;
        let field_nullability = self
            .fields
            .iter()
            .map(|field| {
                if field.nullable() {
                    quote::quote! { ::squealy::NullableColumn }
                } else {
                    quote::quote! { ::squealy::NonNullableColumn }
                }
            })
            .collect::<Vec<_>>();
        let insert_column_key_impls = self
            .fields
            .iter()
            .zip(expr_kind_idents.iter())
            .filter(|(field, _)| field.insertable())
            .map(|(_, expr_kind_ident)| {
                quote::quote! {
                    impl ::squealy::InsertColumnKey for #expr_kind_ident {}
                }
            });
        let update_column_key_impls = self
            .fields
            .iter()
            .zip(expr_kind_idents.iter())
            .filter(|(field, _)| field.updateable())
            .map(|(_, expr_kind_ident)| {
                quote::quote! {
                    impl ::squealy::UpdateColumnKey for #expr_kind_ident {}
                }
            });
        // A nullable column's kind is `NullableExpr`, which gates the `is_null` / `is_not_null`
        // predicate builders so they are only callable on columns that can actually be NULL.
        let nullable_expr_impls = self
            .fields
            .iter()
            .zip(expr_kind_idents.iter())
            .filter(|(field, _)| field.nullable())
            .map(|(_, expr_kind_ident)| {
                quote::quote! {
                    impl ::squealy::NullableExpr for #expr_kind_ident {}
                }
            });

        quote::quote! {
            #(#fk_type_assertions)*
            #(#foreign_key_defs)*
            #(#column_defs)*
            #(#index_defs)*

            #(
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                #visibility enum #expr_kind_idents {}

                impl ::squealy::ExprKind for #expr_kind_idents {
                    type Value = #field_value_tys;
                }

                impl ::squealy::ColumnKey for #expr_kind_idents {
                    type Table = #ident <'static, ::squealy::ColumnExpr>;
                    type Nullability = #field_nullability;

                    const NAME: &'static str = #field_literals;
                }

                impl ::squealy::ProjectionShape for #expr_kind_idents {
                    type Exprs<'scope> = ::squealy::ColumnRef<'scope, #expr_kind_idents>;
                    type ReboundExprs<'scope> = ::squealy::Expr<'scope, #expr_kind_idents>;
                    type Row = #projection_row_value_tys;

                    fn exprs<'scope>(alias: ::squealy::SourceAlias) -> Self::Exprs<'scope> {
                        ::squealy::ColumnRef::column(alias, #field_literals)
                    }

                    fn rebound_exprs<'scope>(alias: ::squealy::SourceAlias) -> Self::ReboundExprs<'scope> {
                        ::squealy::Expr::column(alias, #field_literals)
                    }
                }
            )*

            #(#insert_column_key_impls)*
            #(#update_column_key_impls)*
            #(#nullable_expr_impls)*

            #[doc(hidden)]
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #visibility struct #exprs_ident <'scope> {
                #( pub #fields: ::squealy::ColumnRef<'scope, #expr_kind_idents>, )*
            }

            #[doc(hidden)]
            #[derive(Clone, Debug, PartialEq)]
            #visibility struct #rebound_exprs_ident <'scope> {
                #( pub #fields: ::squealy::Expr<'scope, #expr_kind_idents>, )*
            }

            #[doc(hidden)]
            #[derive(Clone, Debug, PartialEq)]
            #visibility struct #row_shape_ident {
                #( pub #fields: #row_field_value_tys, )*
            }

            #[doc(hidden)]
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #visibility struct #nullable_exprs_ident <'scope> {
                #( pub #fields: ::squealy::ColumnRef<'scope, ::squealy::Nullable<#expr_kind_idents>>, )*
            }

            #[doc(hidden)]
            #[derive(Clone, Debug, PartialEq)]
            #visibility struct #nullable_rebound_exprs_ident <'scope> {
                #( pub #fields: ::squealy::Expr<'scope, ::squealy::Nullable<#expr_kind_idents>>, )*
            }

            #write_builder_defs

            static #columns_static: [&'static dyn ::squealy::Column; #columns_len] = [#( &#column_idents, )*];
            static #indexes_static: [&'static dyn ::squealy::Index; #indexes_len] = [#( &#index_idents, )*];
            #primary_key_static_def
            #uniques_static_def

            impl<'scope, C: ::squealy::ColumnMode> ::squealy::Table for #ident <'scope, C> {
                fn schema_name(&self) -> Option<&'static str> {
                    <Self as ::squealy::SchemaTable>::schema_name()
                }

                fn name(&self) -> &'static str {
                    <Self as ::squealy::SchemaTable>::name()
                }

                fn columns(&self) -> &'static [&'static dyn ::squealy::Column] {
                    <Self as ::squealy::SchemaTable>::columns()
                }

                fn indexes(&self) -> &'static [&'static dyn ::squealy::Index] {
                    <Self as ::squealy::SchemaTable>::indexes()
                }

                #table_primary_key_method
                #table_uniques_method
            }

            impl<'scope, C: ::squealy::ColumnMode> ::squealy::SchemaTable for #ident <'scope, C> {
                type Schema = #schema;

                type WithColumn<'next_scope, NextC: ::squealy::ColumnMode> = #ident <'next_scope, NextC>
                where
                    NextC: 'next_scope;

                type Exprs<'next_scope> = #exprs_ident <'next_scope>;

                type NullableExprs<'next_scope> = #nullable_exprs_ident <'next_scope>;

                fn name() -> &'static str {
                    #name
                }

                fn columns() -> &'static [&'static dyn ::squealy::Column] {
                    &#columns_static
                }

                fn indexes() -> &'static [&'static dyn ::squealy::Index] {
                    &#indexes_static
                }

                #schema_primary_key_method
                #schema_uniques_method

                fn column_names() -> Self::WithColumn<'static, ::squealy::ColumnName> {
                    #ident { #( #fields: #field_literals, )* }
                }

                fn column_exprs_from<'next_scope>(
                    alias: ::squealy::SourceAlias,
                    columns: &Self::WithColumn<'static, ::squealy::ColumnName>,
                ) -> Self::Exprs<'next_scope> {
                    #exprs_ident { #( #fields: ::squealy::ColumnRef::column(alias, columns.#fields), )* }
                }

                fn nullable_column_exprs_from<'next_scope>(
                    alias: ::squealy::SourceAlias,
                    columns: &Self::WithColumn<'static, ::squealy::ColumnName>,
                ) -> Self::NullableExprs<'next_scope> {
                    #nullable_exprs_ident { #( #fields: ::squealy::ColumnRef::column(alias, columns.#fields), )* }
                }
            }

            impl ::squealy::ProjectionShape for #row_shape_ident {
                type Exprs<'scope> = #exprs_ident <'scope>;
                type ReboundExprs<'scope> = #rebound_exprs_ident <'scope>;
                type Row = #row_shape_ident;

                fn exprs<'scope>(alias: ::squealy::SourceAlias) -> Self::Exprs<'scope> {
                    <#ident <'static, ::squealy::ColumnExpr> as ::squealy::SchemaTable>::column_exprs(alias)
                }

                fn rebound_exprs<'scope>(alias: ::squealy::SourceAlias) -> Self::ReboundExprs<'scope> {
                    ::squealy::Projectable::re_alias(
                        &<#ident <'static, ::squealy::ColumnExpr> as ::squealy::SchemaTable>::column_exprs(alias),
                        alias,
                    )
                }
            }

            impl<Backend> ::squealy::Decode<Backend> for #ident <'static, ::squealy::ColumnValue>
            where
                Backend: ::squealy::Backend,
                #(#field_value_tys: ::squealy::Decode<Backend>,)*
            {
                fn decode(
                    row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
                ) -> ::std::result::Result<Self, <Backend as ::squealy::Backend>::Error> {
                    Ok(#ident {
                        #(
                            #fields: ::squealy::RowReader::read::<#field_value_tys>(row)?,
                        )*
                    })
                }
            }

            impl<Backend> ::squealy::Decode<Backend> for #row_shape_ident
            where
                Backend: ::squealy::Backend,
                #(#row_field_decode_bounds,)*
            {
                fn decode(
                    row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
                ) -> ::std::result::Result<Self, <Backend as ::squealy::Backend>::Error> {
                    Ok(#row_shape_ident {
                        #(
                            #fields: #row_field_decode_values,
                        )*
                    })
                }
            }

            impl<Backend> ::squealy::Decode<Backend> for #ident <'static, ::squealy::ColumnNullableValue>
            where
                Backend: ::squealy::Backend,
                #(#field_value_tys: ::squealy::DecodeNullable<Backend>,)*
            {
                fn decode(
                    row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
                ) -> ::std::result::Result<Self, <Backend as ::squealy::Backend>::Error> {
                    Ok(#ident {
                        #(
                            #fields: <#field_value_tys as ::squealy::DecodeNullable<Backend>>::decode_nullable(row)?,
                        )*
                    })
                }
            }

            impl ::squealy::InsertableTable for #ident <'static, ::squealy::ColumnExpr> {}

            impl ::squealy::UpdateableTable for #ident <'static, ::squealy::ColumnExpr> {}

            #write_table_impl

            impl<'scope> ::squealy::Projectable for #exprs_ident <'scope> {
                type Rebound<'next_scope> = #rebound_exprs_ident <'next_scope>;

                fn re_alias<'next_scope>(&self, alias: ::squealy::SourceAlias) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: ::squealy::SourceAlias,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, ::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope, RenderBackend> ::squealy::RenderProjectable<RenderBackend> for #exprs_ident <'scope>
            where
                RenderBackend: ::squealy::Backend,
            {
                fn visit_projection<V>(&self, visitor: &mut V) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_column(self.#fields, ::std::borrow::Cow::Borrowed(#field_literals))?;
                    )*
                    Ok(())
                }

                fn visit_projection_with_prefix<V>(
                    &self,
                    prefix: &str,
                    visitor: &mut V,
                ) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_column(
                            self.#fields,
                            ::std::borrow::Cow::Owned(::std::format!("{prefix}_{}", #field_literals)),
                        )?;
                    )*
                    Ok(())
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #exprs_ident <'scope> {
                type Shape = #row_shape_ident;
            }

            impl<'scope> ::squealy::Projectable for #rebound_exprs_ident <'scope> {
                type Rebound<'next_scope> = #rebound_exprs_ident <'next_scope>;

                fn re_alias<'next_scope>(&self, alias: ::squealy::SourceAlias) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: ::squealy::SourceAlias,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, ::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope, RenderBackend> ::squealy::RenderProjectable<RenderBackend> for #rebound_exprs_ident <'scope>
            where
                RenderBackend: ::squealy::Backend,
            {
                fn visit_projection<V>(&self, visitor: &mut V) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_expr(&self.#fields, ::std::borrow::Cow::Borrowed(#field_literals))?;
                    )*
                    Ok(())
                }

                fn visit_projection_with_prefix<V>(
                    &self,
                    prefix: &str,
                    visitor: &mut V,
                ) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_expr(
                            &self.#fields,
                            ::std::borrow::Cow::Owned(::std::format!("{prefix}_{}", #field_literals)),
                        )?;
                    )*
                    Ok(())
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #rebound_exprs_ident <'scope> {
                type Shape = #row_shape_ident;
            }

            impl<'scope> ::squealy::Projectable for #nullable_exprs_ident <'scope> {
                type Rebound<'next_scope> = #nullable_rebound_exprs_ident <'next_scope>;

                fn re_alias<'next_scope>(&self, alias: ::squealy::SourceAlias) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: ::squealy::SourceAlias,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, ::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope, RenderBackend> ::squealy::RenderProjectable<RenderBackend> for #nullable_exprs_ident <'scope>
            where
                RenderBackend: ::squealy::Backend,
            {
                fn visit_projection<V>(&self, visitor: &mut V) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_column(self.#fields, ::std::borrow::Cow::Borrowed(#field_literals))?;
                    )*
                    Ok(())
                }

                fn visit_projection_with_prefix<V>(
                    &self,
                    prefix: &str,
                    visitor: &mut V,
                ) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_column(
                            self.#fields,
                            ::std::borrow::Cow::Owned(::std::format!("{prefix}_{}", #field_literals)),
                        )?;
                    )*
                    Ok(())
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #nullable_exprs_ident <'scope> {
                type Shape = ::squealy::Maybe<#ident <'static, ::squealy::ColumnExpr>>;
            }

            impl<'scope> ::squealy::Projectable for #nullable_rebound_exprs_ident <'scope> {
                type Rebound<'next_scope> = #nullable_rebound_exprs_ident <'next_scope>;

                fn re_alias<'next_scope>(&self, alias: ::squealy::SourceAlias) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: ::squealy::SourceAlias,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, ::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope, RenderBackend> ::squealy::RenderProjectable<RenderBackend> for #nullable_rebound_exprs_ident <'scope>
            where
                RenderBackend: ::squealy::Backend,
            {
                fn visit_projection<V>(&self, visitor: &mut V) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_expr(&self.#fields, ::std::borrow::Cow::Borrowed(#field_literals))?;
                    )*
                    Ok(())
                }

                fn visit_projection_with_prefix<V>(
                    &self,
                    prefix: &str,
                    visitor: &mut V,
                ) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_expr(
                            &self.#fields,
                            ::std::borrow::Cow::Owned(::std::format!("{prefix}_{}", #field_literals)),
                        )?;
                    )*
                    Ok(())
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #nullable_rebound_exprs_ident <'scope> {
                type Shape = ::squealy::Maybe<#ident <'static, ::squealy::ColumnExpr>>;
            }
        }
        .into()
    }
}

struct BuilderExpansion {
    definitions: TokenStream2,
    table_impl: TokenStream2,
}

impl TableStruct {
    fn write_builder_tokens(
        &self,
        table_ident: &proc_macro2::Ident,
        expr_kind_idents: &[proc_macro2::Ident],
    ) -> BuilderExpansion {
        let visibility = &self.visibility;
        let builder_ident = generated_ident(table_ident, "write", "Builder");
        let update_ready_ident = generated_ident(table_ident, "write", "UpdateReady");
        let fields = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.insertable() || field.updateable())
            .collect::<Vec<_>>();
        let insert_state_idents = fields
            .iter()
            .map(|(_, field)| {
                generated_ident(table_ident, &field.ident.to_string(), "WriteInsertState")
            })
            .collect::<Vec<_>>();
        let insert_missing_idents = fields
            .iter()
            .map(|(_, field)| {
                generated_ident(table_ident, &field.ident.to_string(), "WriteInsertMissing")
            })
            .collect::<Vec<_>>();
        let insert_set_idents = fields
            .iter()
            .map(|(_, field)| {
                generated_ident(table_ident, &field.ident.to_string(), "WriteInsertSet")
            })
            .collect::<Vec<_>>();
        let update_state_idents = fields
            .iter()
            .map(|(_, field)| {
                generated_ident(table_ident, &field.ident.to_string(), "WriteUpdateState")
            })
            .collect::<Vec<_>>();
        let update_missing_idents = fields
            .iter()
            .map(|(_, field)| {
                generated_ident(table_ident, &field.ident.to_string(), "WriteUpdateMissing")
            })
            .collect::<Vec<_>>();
        let update_set_idents = fields
            .iter()
            .map(|(_, field)| {
                generated_ident(table_ident, &field.ident.to_string(), "WriteUpdateSet")
            })
            .collect::<Vec<_>>();
        let insert_state_defaults = insert_state_idents
            .iter()
            .zip(insert_missing_idents.iter())
            .map(|(state, missing)| quote::quote! { #state = #missing })
            .collect::<Vec<_>>();
        let update_state_defaults = update_state_idents
            .iter()
            .zip(update_missing_idents.iter())
            .map(|(state, missing)| quote::quote! { #state = #missing })
            .collect::<Vec<_>>();
        let insert_initial_states = insert_missing_idents.iter().collect::<Vec<_>>();
        let update_initial_states = update_missing_idents.iter().collect::<Vec<_>>();
        let insert_execute_states = fields
            .iter()
            .zip(insert_state_idents.iter())
            .zip(insert_set_idents.iter())
            .map(|(((_, field), state), set)| {
                if field.required_insert() {
                    quote::quote! { #set }
                } else {
                    quote::quote! { #state }
                }
            })
            .collect::<Vec<_>>();
        let insert_execute_state_params = fields
            .iter()
            .zip(insert_state_idents.iter())
            .filter_map(|((_, field), state)| (!field.required_insert()).then_some(state))
            .collect::<Vec<_>>();
        let insert_state_tuple = if insert_state_idents.is_empty() {
            quote::quote! { () }
        } else {
            quote::quote! { (#(#insert_state_idents,)*) }
        };
        let update_state_tuple = if update_state_idents.is_empty() {
            quote::quote! { () }
        } else {
            quote::quote! { (#(#update_state_idents,)*) }
        };
        let update_ready_impls = (1usize..(1usize << fields.len()))
            .filter_map(|mask| {
                let mut has_update_set = false;
                let states = fields
                    .iter()
                    .zip(update_missing_idents.iter())
                    .zip(update_set_idents.iter())
                    .enumerate()
                    .map(|(index, (((_, field), missing), set))| {
                        if field.updateable() && (mask & (1usize << index)) != 0 {
                            has_update_set = true;
                            quote::quote! { #set }
                        } else {
                            quote::quote! { #missing }
                        }
                    })
                    .collect::<Vec<_>>();

                has_update_set.then(|| {
                    quote::quote! {
                        impl #update_ready_ident for (#(#states,)*) {}
                    }
                })
            })
            .collect::<Vec<_>>();
        let state_params = insert_state_idents
            .iter()
            .chain(update_state_idents.iter())
            .collect::<Vec<_>>();
        let setters = fields
            .iter()
            .enumerate()
            .map(|(setter_index, (field_index, field))| {
                let field_ident =
                    proc_macro2::Ident::new(&field.ident.to_string(), Span::call_site());
                let field_expr_kind = &expr_kind_idents[*field_index];
                let value_bound = if field.nullable() {
                    quote::quote! { ::squealy::IntoNullableAssignmentValue<#field_expr_kind> }
                } else {
                    quote::quote! { ::squealy::IntoAssignmentValue<#field_expr_kind> }
                };
                let assignment_value_ty = if field.nullable() {
                    quote::quote! { <Value as ::squealy::IntoNullableAssignmentValue<#field_expr_kind>>::Value }
                } else {
                    quote::quote! { <Value as ::squealy::IntoAssignmentValue<#field_expr_kind>>::Value }
                };
                let assignment_value = if field.nullable() {
                    quote::quote! { ::squealy::IntoNullableAssignmentValue::into_nullable_assignment_value(value) }
                } else {
                    quote::quote! { ::squealy::IntoAssignmentValue::into_assignment_value(value) }
                };
                let insert_assignment_ty = quote::quote! {
                    ::squealy::InsertAssignment<#field_expr_kind, #assignment_value_ty>
                };
                let update_assignment_ty = quote::quote! {
                    ::squealy::UpdateAssignment<#field_expr_kind, #assignment_value_ty>
                };
                let insert_return_states = insert_state_idents
                    .iter()
                    .enumerate()
                    .map(|(index, state)| {
                        if index == setter_index && field.insertable() {
                            let set = &insert_set_idents[index];
                            quote::quote! { #set }
                        } else {
                            quote::quote! { #state }
                        }
                    })
                    .collect::<Vec<_>>();
                let update_return_states = update_state_idents
                    .iter()
                    .enumerate()
                    .map(|(index, state)| {
                        if index == setter_index && field.updateable() {
                            let set = &update_set_idents[index];
                            quote::quote! { #set }
                        } else {
                            quote::quote! { #state }
                        }
                    })
                    .collect::<Vec<_>>();
                let insert_columns_out = if field.insertable() {
                    quote::quote! { <InsertColumns as ::squealy::PushBack<#insert_assignment_ty>>::Output }
                } else {
                    quote::quote! { InsertColumns }
                };
                let update_columns_out = if field.updateable() {
                    quote::quote! { <UpdateColumns as ::squealy::PushBack<#update_assignment_ty>>::Output }
                } else {
                    quote::quote! { UpdateColumns }
                };
                let insert_bound = field
                    .insertable()
                    .then(|| quote::quote! { InsertColumns: ::squealy::PushBack<#insert_assignment_ty>, });
                let update_bound = field
                    .updateable()
                    .then(|| quote::quote! { UpdateColumns: ::squealy::PushBack<#update_assignment_ty>, });
                let insert_append = if field.insertable() {
                    let value = if field.updateable() {
                        quote::quote! { assignment_value.clone() }
                    } else {
                        quote::quote! { assignment_value }
                    };
                    quote::quote! {
                        let insert_columns = self.insert_columns.push_back(
                            ::squealy::InsertAssignment::<#field_expr_kind, #assignment_value_ty>::new(#value)
                        );
                    }
                } else {
                    quote::quote! {
                        let insert_columns = self.insert_columns;
                    }
                };
                let update_append = if field.updateable() {
                    quote::quote! {
                        let update_columns = self.update_columns.push_back(
                            ::squealy::UpdateAssignment::<#field_expr_kind, #assignment_value_ty>::new(assignment_value)
                        );
                    }
                } else {
                    quote::quote! {
                        let update_columns = self.update_columns;
                    }
                };

                quote::quote! {
                    impl<'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#state_params),*>
                        #builder_ident <'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#state_params),*>
                    where
                        Conn: ::squealy::QueryBuilder + 'conn,
                    {
                        pub fn #field_ident<Value>(
                            self,
                            value: Value,
                        ) -> #builder_ident <
                            'conn,
                            Conn,
                            #insert_columns_out,
                            #update_columns_out,
                            Filters,
                            FilterState,
                            #(#insert_return_states,)*
                            #(#update_return_states),*
                        >
                        where
                            Value: #value_bound,
                            #insert_bound
                            #update_bound
                        {
                            let assignment_value = #assignment_value;
                            #insert_append
                            #update_append
                            #builder_ident {
                                connection: self.connection,
                                insert_columns,
                                update_columns,
                                filters: self.filters,
                                _state: ::std::marker::PhantomData,
                            }
                        }
                    }
                }
            })
            .collect::<Vec<_>>();
        let update_finalizers = if fields.iter().any(|(_, field)| field.updateable()) {
            quote::quote! {
                impl<'conn, Conn, InsertColumns, UpdateColumns, Filters, #(#insert_state_idents,)* #(#update_state_idents),*>
                    #builder_ident <'conn, Conn, InsertColumns, UpdateColumns, Filters, ::squealy::MutationFiltered, #(#insert_state_idents,)* #(#update_state_idents),*>
                where
                    Conn: ::squealy::QueryBuilder + 'conn,
                    UpdateColumns: ::squealy::UpdateAssignments + 'conn,
                    Filters: ::squealy::PredicateNodes + 'conn,
                    #update_state_tuple: #update_ready_ident,
                {
                    pub fn update(
                        self,
                    ) -> impl ::std::future::Future<
                        Output = ::std::result::Result<u64, <<Conn as ::squealy::QueryBuilder>::Backend as ::squealy::Backend>::Error>,
                    > + 'conn
                    where
                        Conn: ::squealy::Connection,
                        UpdateColumns: ::squealy::UpdateAssignments,
                        <UpdateColumns as ::squealy::UpdateAssignments>::Params: ::squealy::NoRuntimeParams,
                        Filters: ::squealy::PredicateNodes,
                        <Filters as ::squealy::PredicateNodes>::Params: ::squealy::NoRuntimeParams,
                        <Conn as ::squealy::QueryBuilder>::Update<
                            'conn,
                            #table_ident <'static, ::squealy::ColumnExpr>,
                            (),
                            UpdateColumns,
                            Filters,
                            (),
                        >: ::squealy::ExecutableUpdateQuery<'conn, UpdateColumns, Filters, ()>,
                    {
                        let query = <<Conn as ::squealy::QueryBuilder>::Update<
                            'conn,
                            #table_ident <'static, ::squealy::ColumnExpr>,
                            (),
                            UpdateColumns,
                            Filters,
                            (),
                        > as ::squealy::UpdateQuery<'conn, UpdateColumns, Filters, ()>>::build(
                            self.connection,
                            Self::ALIAS,
                            self.update_columns,
                            self.filters,
                            (),
                        );
                        async move {
                            ::squealy::ExecutableUpdateQuery::execute(&query).await
                        }
                    }

                    pub fn update_returning<P>(
                        self,
                        projection: impl ::std::ops::FnOnce(
                            <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
                        ) -> P,
                    ) -> <Conn as ::squealy::QueryBuilder>::Update<'conn, #table_ident <'static, ::squealy::ColumnExpr>, <P as ::squealy::ReturningProjection<'static>>::Shape, UpdateColumns, Filters, P>
                    where
                        P: ::squealy::ReturningProjection<'static> + ::squealy::Projectable,
                        <P::Shape as ::squealy::ProjectionShape>::Row: ::squealy::Decode<<Conn as ::squealy::QueryBuilder>::Backend>,
                    {
                        let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
                        let projection = projection(table);
                        <<Conn as ::squealy::QueryBuilder>::Update<
                            'conn,
                            #table_ident <'static, ::squealy::ColumnExpr>,
                            <P as ::squealy::ReturningProjection<'static>>::Shape,
                            UpdateColumns,
                            Filters,
                            P,
                        > as ::squealy::UpdateQuery<'conn, UpdateColumns, Filters, P>>::build(
                            self.connection,
                            Self::ALIAS,
                            self.update_columns,
                            self.filters,
                            projection,
                        )
                    }
                }
            }
        } else {
            quote::quote! {}
        };

        let definitions = quote::quote! {
            #(
                #[doc(hidden)]
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                #visibility struct #insert_missing_idents;
            )*

            #(
                #[doc(hidden)]
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                #visibility struct #insert_set_idents;
            )*

            #(
                #[doc(hidden)]
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                #visibility struct #update_missing_idents;
            )*

            #(
                #[doc(hidden)]
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                #visibility struct #update_set_idents;
            )*

            #[doc(hidden)]
            #visibility trait #update_ready_ident {}
            #(#update_ready_impls)*

            #[doc(hidden)]
            #[derive(Clone, Debug, PartialEq)]
            #visibility struct #builder_ident <
                'conn,
                Conn: ::squealy::QueryBuilder + 'conn,
                InsertColumns = ::squealy::HNil,
                UpdateColumns = ::squealy::HNil,
                Filters = ::squealy::HNil,
                FilterState = ::squealy::MutationUnfiltered,
                #(#insert_state_defaults,)*
                #(#update_state_defaults),*
            > {
                connection: &'conn Conn,
                insert_columns: InsertColumns,
                update_columns: UpdateColumns,
                filters: Filters,
                _state: ::std::marker::PhantomData<(FilterState, #insert_state_tuple, #update_state_tuple)>,
            }

            impl<'conn, Conn> #builder_ident <'conn, Conn, ::squealy::HNil, ::squealy::HNil, ::squealy::HNil, ::squealy::MutationUnfiltered, #(#insert_initial_states,)* #(#update_initial_states),*>
            where
                Conn: ::squealy::QueryBuilder + 'conn,
            {
                fn new(connection: &'conn Conn) -> Self {
                    Self {
                        connection,
                        insert_columns: ::squealy::HNil,
                        update_columns: ::squealy::HNil,
                        filters: ::squealy::HNil,
                        _state: ::std::marker::PhantomData,
                    }
                }
            }

            impl<'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#state_params),*>
                #builder_ident <'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#state_params),*>
            where
                Conn: ::squealy::QueryBuilder + 'conn,
            {
                const ALIAS: ::squealy::SourceAlias = ::squealy::SourceAlias::new(0, 0);
            }

            impl<'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#state_params),*>
                #builder_ident <'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#state_params),*>
            where
                Conn: ::squealy::QueryBuilder + 'conn,
            {
                pub fn where_<P, PredicateAst>(
                    self,
                    predicate: impl ::std::ops::FnOnce(
                        &<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
                    ) -> ::squealy::Predicate<'static, P, PredicateAst>,
                ) -> #builder_ident <'conn, Conn, InsertColumns, UpdateColumns, <Filters as ::squealy::PushBack<::squealy::Predicate<'static, P, PredicateAst>>>::Output, ::squealy::MutationFiltered, #(#state_params),*>
                where
                    Filters: ::squealy::PushBack<::squealy::Predicate<'static, P, PredicateAst>>,
                    <Filters as ::squealy::PushBack<::squealy::Predicate<'static, P, PredicateAst>>>::Output: ::squealy::PredicateNodes,
                    P: ::squealy::PredicateKind,
                    PredicateAst: ::squealy::PredicateAst,
                {
                    let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
                    let predicate = predicate(&table);
                    #builder_ident {
                        connection: self.connection,
                        insert_columns: self.insert_columns,
                        update_columns: self.update_columns,
                        filters: self.filters.push_back(predicate),
                        _state: ::std::marker::PhantomData,
                    }
                }
            }

            impl<'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#state_params),*>
                #builder_ident <'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#state_params),*>
            where
                Conn: ::squealy::QueryBuilder + 'conn,
            {
                pub fn all(self) -> #builder_ident <'conn, Conn, InsertColumns, UpdateColumns, Filters, ::squealy::MutationFiltered, #(#state_params),*> {
                    #builder_ident {
                        connection: self.connection,
                        insert_columns: self.insert_columns,
                        update_columns: self.update_columns,
                        filters: self.filters,
                        _state: ::std::marker::PhantomData,
                    }
                }
            }

            #(#setters)*

            impl<'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#insert_execute_state_params,)* #(#update_state_idents),*> #builder_ident <'conn, Conn, InsertColumns, UpdateColumns, Filters, FilterState, #(#insert_execute_states,)* #(#update_state_idents),*>
            where
                Conn: ::squealy::QueryBuilder + 'conn,
                InsertColumns: ::squealy::InsertAssignments + 'conn,
                <InsertColumns as ::squealy::InsertAssignments>::Params: ::squealy::HAppend<::squealy::HNil>,
            {
                pub fn insert(
                    self,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<u64, <<Conn as ::squealy::QueryBuilder>::Backend as ::squealy::Backend>::Error>,
                > + 'conn
                where
                    Conn: ::squealy::Connection,
                    ::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil>: ::squealy::InsertRows,
                    <::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil> as ::squealy::InsertRows>::Params: ::squealy::NoRuntimeParams,
                    <Conn as ::squealy::QueryBuilder>::Insert<
                        'conn,
                        #table_ident <'static, ::squealy::ColumnExpr>,
                        (),
                        ::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil>,
                        (),
                    >: ::squealy::ExecutableInsertQuery<'conn, ::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil>, ()>,
                {
                    let insert_rows = ::squealy::HCons {
                        head: ::squealy::InsertRow::new(self.insert_columns),
                        tail: ::squealy::HNil,
                    };
                    let query = <<Conn as ::squealy::QueryBuilder>::Insert<
                        'conn,
                        #table_ident <'static, ::squealy::ColumnExpr>,
                        (),
                        ::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil>,
                        (),
                    > as ::squealy::InsertQuery<'conn, ::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil>, ()>>::build(
                        self.connection,
                        insert_rows,
                        (),
                    );
                    async move {
                        ::squealy::ExecutableInsertQuery::execute(&query).await
                    }
                }

                pub fn insert_returning<P>(
                    self,
                    projection: impl ::std::ops::FnOnce(
                        <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
                    ) -> P,
                ) -> <Conn as ::squealy::QueryBuilder>::Insert<'conn, #table_ident <'static, ::squealy::ColumnExpr>, <P as ::squealy::ReturningProjection<'static>>::Shape, ::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil>, P>
                where
                    P: ::squealy::ReturningProjection<'static> + ::squealy::Projectable,
                    <P::Shape as ::squealy::ProjectionShape>::Row: ::squealy::Decode<<Conn as ::squealy::QueryBuilder>::Backend>,
                {
                    let insert_rows = ::squealy::HCons {
                        head: ::squealy::InsertRow::new(self.insert_columns),
                        tail: ::squealy::HNil,
                    };
                    let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
                    let projection = projection(table);
                    <<Conn as ::squealy::QueryBuilder>::Insert<
                        'conn,
                        #table_ident <'static, ::squealy::ColumnExpr>,
                        <P as ::squealy::ReturningProjection<'static>>::Shape,
                        ::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil>,
                        P,
                    > as ::squealy::InsertQuery<'conn, ::squealy::HCons<::squealy::InsertRow<InsertColumns>, ::squealy::HNil>, P>>::build(
                        self.connection,
                        insert_rows,
                        projection,
                    )
                }
            }

            #update_finalizers
        };

        let table_impl = quote::quote! {
            impl ::squealy::WriteableTable for #table_ident <'static, ::squealy::ColumnExpr> {
                type WriteBuilder<'conn, Conn> = #builder_ident <'conn, Conn>
                where
                    Conn: ::squealy::QueryBuilder + 'conn;

                fn write_builder<'conn, Conn>(
                    connection: &'conn Conn,
                ) -> Self::WriteBuilder<'conn, Conn>
                where
                    Conn: ::squealy::QueryBuilder + 'conn,
                {
                    #builder_ident::new(connection)
                }
            }
        };

        BuilderExpansion {
            definitions,
            table_impl,
        }
    }
}

impl IndexAttrs {
    fn index_definition_tokens(
        &self,
        index_ident: &proc_macro2::Ident,
        fields: &[Field],
    ) -> proc_macro2::TokenStream {
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
            struct #index_ident;

            impl ::squealy::Index for #index_ident {
                fn name(&self) -> Option<&'static str> {
                    #name
                }

                fn columns(&self) -> &'static [&'static str] {
                    &[#( #columns, )*]
                }

                fn unique(&self) -> bool {
                    #unique
                }
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

    fn insertable(&self) -> bool {
        self.attrs
            .insert
            .unwrap_or(!self.attrs.generated && !self.attrs.auto_increment)
    }

    fn required_insert(&self) -> bool {
        self.insertable() && !self.nullable() && self.attrs.default.is_none()
    }

    fn updateable(&self) -> bool {
        self.attrs
            .update
            .unwrap_or(!self.attrs.generated && !self.attrs.auto_increment)
    }

    fn nullable(&self) -> bool {
        self.attrs.nullable.unwrap_or(false)
    }

    fn default_tokens(&self) -> proc_macro2::TokenStream {
        let Some(default) = self.attrs.default.as_ref() else {
            return quote::quote! { None };
        };

        let default = match default {
            DefaultAttrs::Literal(DefaultLiteral::Null) => {
                quote::quote! { ::squealy::ColumnDefault::Null }
            }
            DefaultAttrs::Literal(DefaultLiteral::Int(value)) => {
                quote::quote! { ::squealy::ColumnDefault::Int(#value) }
            }
            DefaultAttrs::Literal(DefaultLiteral::UInt(value)) => {
                quote::quote! { ::squealy::ColumnDefault::UInt(#value) }
            }
            DefaultAttrs::Literal(DefaultLiteral::Float(value)) => {
                quote::quote! { ::squealy::ColumnDefault::Float(#value) }
            }
            DefaultAttrs::Literal(DefaultLiteral::Text(value)) => {
                let value = Literal::string(value);
                quote::quote! { ::squealy::ColumnDefault::Text(#value) }
            }
            DefaultAttrs::Literal(DefaultLiteral::Bool(value)) => {
                let value = bool_tokens(*value);
                quote::quote! { ::squealy::ColumnDefault::Bool(#value) }
            }
            DefaultAttrs::CurrentTimestamp => {
                quote::quote! { ::squealy::ColumnDefault::CurrentTimestamp }
            }
            DefaultAttrs::CurrentDate => {
                quote::quote! { ::squealy::ColumnDefault::CurrentDate }
            }
            DefaultAttrs::CurrentTime => {
                quote::quote! { ::squealy::ColumnDefault::CurrentTime }
            }
            DefaultAttrs::Raw(value) => {
                let value = Literal::string(value);
                quote::quote! { ::squealy::ColumnDefault::Raw(#value) }
            }
        };

        quote::quote! { Some(#default) }
    }

    fn column_definition_tokens(
        &self,
        column_ident: &proc_macro2::Ident,
    ) -> proc_macro2::TokenStream {
        let name = Literal::string(&self.column_name());
        let primary_key = bool_tokens(self.attrs.primary_key);
        let indexed = bool_tokens(self.attrs.index);
        let unique = bool_tokens(self.attrs.unique);
        let nullable = bool_tokens(self.attrs.nullable.unwrap_or(false));
        let auto_increment = bool_tokens(self.attrs.auto_increment);
        let generated = bool_tokens(self.attrs.generated);
        let insertable = bool_tokens(self.insertable());
        let updateable = bool_tokens(self.updateable());
        let default = self.default_tokens();
        let value_ty = &self.value_ty;
        let column_type = if let Some(db_type) = self.attrs.db_type.as_deref() {
            parse_db_type(db_type)
        } else {
            quote::quote! { <#value_ty as ::squealy::HasColumnType>::COLUMN_TYPE }
        };
        let check = option_literal(self.attrs.check.as_deref());
        let references = self.references_tokens(column_ident);

        quote::quote! {
            struct #column_ident;

            impl ::squealy::Column for #column_ident {
                fn name(&self) -> &'static str { #name }
                fn primary_key(&self) -> bool { #primary_key }
                fn indexed(&self) -> bool { #indexed }
                fn unique(&self) -> bool { #unique }
                fn nullable(&self) -> bool { #nullable }
                fn auto_increment(&self) -> bool { #auto_increment }
                fn generated(&self) -> bool { #generated }
                fn insertable(&self) -> bool { #insertable }
                fn updateable(&self) -> bool { #updateable }
                fn default(&self) -> Option<::squealy::ColumnDefault> { #default }
                fn column_type(&self) -> ::squealy::ColumnType { #column_type }
                fn check(&self) -> Option<&'static str> { #check }
                fn references(&self) -> Option<&'static dyn ::squealy::ForeignKey> { #references }
            }
        }
    }

    fn index_definition_tokens(
        &self,
        index_ident: &proc_macro2::Ident,
    ) -> proc_macro2::TokenStream {
        let name = option_literal(None);
        let column_name = Literal::string(&self.column_name());
        let unique = bool_tokens(self.attrs.unique);

        quote::quote! {
            struct #index_ident;

            impl ::squealy::Index for #index_ident {
                fn name(&self) -> Option<&'static str> {
                    #name
                }

                fn columns(&self) -> &'static [&'static str] {
                    &[#column_name]
                }

                fn unique(&self) -> bool {
                    #unique
                }
            }
        }
    }

    fn foreign_key_definition_tokens(
        &self,
        column_ident: &proc_macro2::Ident,
    ) -> Option<proc_macro2::TokenStream> {
        let references = self.attrs.references.as_ref()?;
        let foreign_key_ident = foreign_key_ident(column_ident);
        let table = proc_macro2::Ident::new(
            &references
                .table
                .as_ref()
                .expect("foreign keys should have a table before code generation")
                .to_string(),
            Span::call_site(),
        );
        let column = proc_macro2::Ident::new(
            &references
                .column
                .as_ref()
                .expect("foreign keys should have a column before code generation")
                .to_string(),
            Span::call_site(),
        );
        let on_delete = option_literal(references.on_delete.as_deref());
        let on_update = option_literal(references.on_update.as_deref());

        Some(quote::quote! {
            struct #foreign_key_ident;

            impl ::squealy::ForeignKey for #foreign_key_ident {
                fn schema_name(&self) -> Option<&'static str> {
                    <#table <'static, ::squealy::ColumnName> as ::squealy::SchemaTable>::schema_name()
                }

                fn table(&self) -> &'static str {
                    <#table <'static, ::squealy::ColumnName> as ::squealy::SchemaTable>::name()
                }

                fn column(&self) -> &'static str {
                    <#table <'static, ::squealy::ColumnName> as ::squealy::SchemaTable>::column_names().#column
                }
                fn on_delete(&self) -> Option<&'static str> { #on_delete }
                fn on_update(&self) -> Option<&'static str> { #on_update }
            }
        })
    }

    fn references_tokens(&self, column_ident: &proc_macro2::Ident) -> proc_macro2::TokenStream {
        if self.attrs.references.is_none() {
            return quote::quote! { None };
        }

        let foreign_key_ident = foreign_key_ident(column_ident);
        quote::quote! { Some(&#foreign_key_ident) }
    }
}

fn table_struct(input: TokenStream) -> Result<TableStruct, MacroError> {
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

    if !has_scope_and_mode(&tokens[struct_index + 2..body_index]) {
        return Err(MacroError::spanned(
            "Table currently requires structs shaped like `Type<'scope, C: ColumnMode = ColumnExpr>`",
            ident.span().into(),
        ));
    }

    let fields = match &tokens[body_index] {
        TokenTree::Group(group) => named_fields(group)?,
        _ => unreachable!(),
    };
    let table_attrs = table_attributes(&tokens[..struct_index])?;
    validate_index_columns(&table_attrs.indexes, &fields)?;
    validate_primary_key(table_attrs.primary_key.as_ref(), &fields)?;
    validate_unique_columns(&table_attrs.uniques, &fields)?;
    validate_field_attrs(&fields)?;

    Ok(TableStruct {
        ident,
        visibility: struct_visibility(&tokens, struct_index),
        fields,
        indexes: table_attrs.indexes,
        primary_key: table_attrs.primary_key,
        uniques: table_attrs.uniques,
        schema: table_attrs.schema,
    })
}

/// Validates a table-level `#[primary_key(columns = [..])]` against the struct's fields:
/// every referenced column must exist and be non-nullable, and it must not be combined with
/// any per-column `#[column(primary_key)]` marker (which is the single-column form).
fn validate_primary_key(
    primary_key: Option<&PrimaryKeyAttrs>,
    fields: &[Field],
) -> Result<(), String> {
    let Some(primary_key) = primary_key else {
        return Ok(());
    };

    if fields.iter().any(|field| field.attrs.primary_key) {
        return Err(
            "a table cannot combine `#[primary_key(...)]` with per-column `#[column(primary_key)]`; \
             use one form"
                .to_owned(),
        );
    }

    for column in &primary_key.columns {
        let column = column.to_string();
        let Some(field) = fields
            .iter()
            .find(|field| field.ident.to_string() == column)
        else {
            return Err(format!("primary key references unknown field `{column}`"));
        };
        if field.attrs.nullable == Some(true) {
            return Err(format!(
                "primary key column `{column}` cannot be `nullable`"
            ));
        }
    }

    Ok(())
}

/// Structurally verifies the generic parameter list contains a `'scope` lifetime
/// and a `C` column-mode type parameter, the shape every table field relies on
/// (`C::Type<'scope, Value>`). This replaces a brittle stringified-token match.
fn has_scope_and_mode(generic_tokens: &[TokenTree]) -> bool {
    let Some(start) = generic_tokens
        .iter()
        .position(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '<'))
    else {
        return false;
    };

    let mut depth = 0usize;
    let mut has_scope = false;
    let mut has_mode = false;

    for (offset, token) in generic_tokens[start..].iter().enumerate() {
        match token {
            TokenTree::Punct(punct) if punct.as_char() == '<' => depth += 1,
            TokenTree::Punct(punct) if punct.as_char() == '>' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            // A lifetime is a `'` punct joined to the following identifier.
            TokenTree::Punct(punct) if punct.as_char() == '\'' && depth == 1 => {
                if let Some(TokenTree::Ident(name)) = generic_tokens.get(start + offset + 1)
                    && name.to_string() == "scope"
                {
                    has_scope = true;
                }
            }
            TokenTree::Ident(ident) if depth == 1 && ident.to_string() == "C" => has_mode = true,
            _ => {}
        }
    }

    has_scope && has_mode
}

fn struct_visibility(tokens: &[TokenTree], struct_index: usize) -> TokenStream2 {
    if struct_index >= 1 && matches_ident(&tokens[struct_index - 1], "pub") {
        let mut visibility = TokenStream::new();
        visibility.extend([tokens[struct_index - 1].clone()]);
        return TokenStream2::from(visibility);
    }

    if struct_index >= 2
        && matches_ident(&tokens[struct_index - 2], "pub")
        && matches!(
            &tokens[struct_index - 1],
            TokenTree::Group(group) if group.delimiter() == Delimiter::Parenthesis
        )
    {
        let mut visibility = TokenStream::new();
        visibility.extend([
            tokens[struct_index - 2].clone(),
            tokens[struct_index - 1].clone(),
        ]);
        return TokenStream2::from(visibility);
    }

    TokenStream2::new()
}

fn table_attributes(tokens: &[TokenTree]) -> Result<TableAttrs, String> {
    let mut attrs = TableAttrs::default();
    let mut iter = tokens.iter();

    while let Some(token) = iter.next() {
        if !is_attribute_start(token) {
            continue;
        }

        let Some(TokenTree::Group(attr)) = iter.next() else {
            return Err("Table attribute is missing its bracketed body".to_owned());
        };
        apply_table_attribute(attr, &mut attrs)?;
    }

    Ok(attrs)
}

fn apply_table_attribute(group: &Group, attrs: &mut TableAttrs) -> Result<(), String> {
    if group.delimiter() != Delimiter::Bracket {
        return Err("Table attributes must use square brackets".to_owned());
    }

    let mut tokens = group.stream().into_iter();
    let Some(TokenTree::Ident(attr_name)) = tokens.next() else {
        return Ok(());
    };

    match attr_name.to_string().as_str() {
        "index" => {
            let Some(TokenTree::Group(meta)) = tokens.next() else {
                return Err(
                    "table-level #[index(...)] requires metadata inside parentheses".to_owned(),
                );
            };
            attrs
                .indexes
                .push(parse_index(meta.stream().into_iter().collect::<Vec<_>>())?);
        }
        "primary_key" => {
            let Some(TokenTree::Group(meta)) = tokens.next() else {
                return Err(
                    "table-level #[primary_key(...)] requires metadata inside parentheses"
                        .to_owned(),
                );
            };
            if attrs.primary_key.is_some() {
                return Err("a table may declare at most one #[primary_key(...)]".to_owned());
            }
            attrs.primary_key = Some(parse_primary_key(
                meta.stream().into_iter().collect::<Vec<_>>(),
            )?);
        }
        "unique" => {
            let Some(TokenTree::Group(meta)) = tokens.next() else {
                return Err(
                    "table-level #[unique(...)] requires metadata inside parentheses".to_owned(),
                );
            };
            attrs
                .uniques
                .push(parse_unique(meta.stream().into_iter().collect::<Vec<_>>())?);
        }
        "schema" => {
            let Some(TokenTree::Group(schema)) = tokens.next() else {
                return Err("table-level #[schema(...)] requires a schema type".to_owned());
            };
            let schema_tokens = schema.stream();
            if schema_tokens.is_empty() {
                return Err("table-level #[schema(...)] requires a schema type".to_owned());
            }
            attrs.schema = Some(proc_macro2::TokenStream::from(schema_tokens));
        }
        _ => {}
    }

    Ok(())
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

fn parse_primary_key(tokens: Vec<TokenTree>) -> Result<PrimaryKeyAttrs, String> {
    let mut index = 0;
    let mut attrs = PrimaryKeyAttrs {
        name: None,
        columns: Vec::new(),
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
            "name" => {
                if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=')
                {
                    return Err("primary key option `name` requires a string value".to_owned());
                }
                index += 1;
                attrs.name = Some(match tokens.get(index) {
                    Some(TokenTree::Literal(literal)) => literal_string(literal),
                    Some(token) => token.to_string(),
                    None => return Err("primary key option `name` is missing a value".to_owned()),
                });
                index += 1;
            }
            "columns" => {
                if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=')
                {
                    return Err(
                        "primary key option `columns` requires a bracketed field list".to_owned(),
                    );
                }
                index += 1;
                let Some(TokenTree::Group(columns)) = tokens.get(index) else {
                    return Err(
                        "primary key option `columns` requires a bracketed field list".to_owned(),
                    );
                };
                if columns.delimiter() != Delimiter::Bracket {
                    return Err("primary key option `columns` requires square brackets".to_owned());
                }
                attrs.columns = parse_index_columns(columns)?;
                index += 1;
            }
            _ => return Err(format!("unsupported primary key option `{name}`")),
        }
    }

    if attrs.columns.is_empty() {
        return Err("table-level #[primary_key(...)] requires at least one column".to_owned());
    }

    Ok(attrs)
}

fn parse_unique(tokens: Vec<TokenTree>) -> Result<UniqueAttrs, String> {
    let mut index = 0;
    let mut attrs = UniqueAttrs {
        name: None,
        columns: Vec::new(),
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
            "name" => {
                if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=')
                {
                    return Err("unique option `name` requires a string value".to_owned());
                }
                index += 1;
                attrs.name = Some(match tokens.get(index) {
                    Some(TokenTree::Literal(literal)) => literal_string(literal),
                    Some(token) => token.to_string(),
                    None => return Err("unique option `name` is missing a value".to_owned()),
                });
                index += 1;
            }
            "columns" => {
                if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=')
                {
                    return Err(
                        "unique option `columns` requires a bracketed field list".to_owned()
                    );
                }
                index += 1;
                let Some(TokenTree::Group(columns)) = tokens.get(index) else {
                    return Err(
                        "unique option `columns` requires a bracketed field list".to_owned()
                    );
                };
                if columns.delimiter() != Delimiter::Bracket {
                    return Err("unique option `columns` requires square brackets".to_owned());
                }
                attrs.columns = parse_index_columns(columns)?;
                index += 1;
            }
            _ => return Err(format!("unsupported unique option `{name}`")),
        }
    }

    if attrs.columns.is_empty() {
        return Err("table-level #[unique(...)] requires at least one column".to_owned());
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

/// Rejects column attribute combinations that are mutually contradictory and would
/// otherwise only surface as a database error at DDL time.
fn validate_field_attrs(fields: &[Field]) -> Result<(), MacroError> {
    for field in fields {
        let attrs = &field.attrs;
        let name = field.ident.to_string();
        let nullable = attrs.nullable == Some(true);
        let at_field = |message: String| MacroError::spanned(message, field.ident.span().into());

        if attrs.primary_key && nullable {
            return Err(at_field(format!(
                "column `{name}` cannot be both `primary_key` and `nullable`"
            )));
        }
        if attrs.auto_increment && nullable {
            return Err(at_field(format!(
                "column `{name}` cannot be both `auto_increment` and `nullable`"
            )));
        }
        if attrs.auto_increment && attrs.default.is_some() {
            return Err(at_field(format!(
                "column `{name}` cannot combine `auto_increment` with a default"
            )));
        }
        if attrs.generated && attrs.default.is_some() {
            return Err(at_field(format!(
                "column `{name}` cannot combine `generated` with a default"
            )));
        }
        if attrs.generated && attrs.auto_increment {
            return Err(at_field(format!(
                "column `{name}` cannot be both `generated` and `auto_increment`"
            )));
        }
    }

    Ok(())
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

fn validate_unique_columns(uniques: &[UniqueAttrs], fields: &[Field]) -> Result<(), String> {
    for unique in uniques {
        for column in &unique.columns {
            let column = column.to_string();
            if !fields.iter().any(|field| field.ident.to_string() == column) {
                return Err(format!(
                    "unique constraint references unknown field `{column}`"
                ));
            }
        }
    }

    Ok(())
}

fn named_fields(group: &Group) -> Result<Vec<Field>, String> {
    let mut fields = Vec::new();
    let mut pending_attrs = FieldAttrs::default();
    let tokens = group.stream().into_iter().collect::<Vec<_>>();
    let mut index = 0;

    while index < tokens.len() {
        let token = tokens[index].clone();
        if is_attribute_start(&token) {
            let Some(TokenTree::Group(attr)) = tokens.get(index + 1) else {
                return Err("Table field attribute is missing its bracketed body".to_owned());
            };
            apply_attribute(attr, &mut pending_attrs)?;
            index += 2;
            continue;
        }

        let TokenTree::Ident(ident) = token else {
            index += 1;
            continue;
        };

        if !matches!(
            tokens.get(index + 1),
            Some(TokenTree::Punct(punct))
                if punct.as_char() == ':' && punct.spacing() == proc_macro::Spacing::Alone
        ) {
            index += 1;
            continue;
        }

        index += 2;
        let mut type_tokens = Vec::new();
        let mut angle_depth = 0usize;

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
                TokenTree::Punct(punct) if punct.as_char() == ',' && angle_depth == 0 => break,
                token => type_tokens.push(token.clone()),
            }
            index += 1;
        }

        fields.push(Field {
            ident,
            value_ty: column_type_value(&type_tokens)?,
            attrs: std::mem::take(&mut pending_attrs),
        });
        index += 1;
    }

    if fields.is_empty() {
        Err("Table requires at least one named field".to_owned())
    } else {
        Ok(fields)
    }
}

fn column_type_value(type_tokens: &[TokenTree]) -> Result<proc_macro2::TokenStream, String> {
    let mut angle_depth = 0usize;
    let mut seen_first_argument = false;
    let mut value_tokens = Vec::new();

    for token in type_tokens {
        match token {
            TokenTree::Punct(punct) if punct.as_char() == '<' => {
                angle_depth += 1;
                if seen_first_argument {
                    value_tokens.push(token.clone());
                }
            }
            TokenTree::Punct(punct) if punct.as_char() == '>' => {
                if seen_first_argument && angle_depth > 1 {
                    value_tokens.push(token.clone());
                }
                angle_depth = angle_depth.saturating_sub(1);
            }
            TokenTree::Punct(punct) if punct.as_char() == ',' && angle_depth == 1 => {
                seen_first_argument = true;
            }
            token if seen_first_argument => value_tokens.push(token.clone()),
            _ => {}
        }
    }

    if value_tokens.is_empty() {
        return Err("Table fields must use `C::Type<'scope, Value>`".to_owned());
    }

    Ok(proc_macro2::TokenStream::from(TokenStream::from_iter(
        value_tokens,
    )))
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
        "generated" => attrs.generated = true,
        "insert" => attrs.insert = Some(required_bool(name, value_tokens)?),
        "update" => attrs.update = Some(required_bool(name, value_tokens)?),
        "default" => attrs.default = Some(parse_default(value_tokens)?),
        "default_raw" => {
            attrs.default = Some(DefaultAttrs::Raw(required_literal(name, value_tokens)?))
        }
        "db_type" => attrs.db_type = Some(required_literal(name, value_tokens)?),
        "check" => attrs.check = Some(required_literal(name, value_tokens)?),
        "column_name" | "name" => attrs.column_name = Some(required_literal(name, value_tokens)?),
        "references" => attrs.references = Some(parse_references(value_tokens)?),
        _ => return Err(format!("unsupported Table field attribute `{name}`")),
    }

    Ok(())
}

fn parse_default(value_tokens: &[TokenTree]) -> Result<DefaultAttrs, String> {
    let Some(token) = value_tokens.first() else {
        return Err(
            "attribute `default` requires `value(...)`, `current_timestamp`, `current_date`, or `current_time`"
                .to_owned(),
        );
    };

    if let TokenTree::Ident(ident) = token {
        return match ident.to_string().as_str() {
            "value" => {
                let Some(TokenTree::Group(group)) = value_tokens.get(1) else {
                    return Err("attribute `default = value(...)` requires a value".to_owned());
                };
                if group.delimiter() != Delimiter::Parenthesis {
                    return Err("attribute `default = value(...)` requires parentheses".to_owned());
                }
                Ok(DefaultAttrs::Literal(parse_default_literal(
                    &group.stream().into_iter().collect::<Vec<_>>(),
                )?))
            }
            "current_timestamp" => Ok(DefaultAttrs::CurrentTimestamp),
            "current_date" => Ok(DefaultAttrs::CurrentDate),
            "current_time" => Ok(DefaultAttrs::CurrentTime),
            _ => Err(format!(
                "unsupported default `{ident}`; use `value(...)`, `current_timestamp`, `current_date`, or `current_time`"
            )),
        };
    }

    Err("attribute `default` uses portable defaults; use `default = value(...)` for literals or `default_raw = \"...\"` for backend-specific SQL".to_owned())
}

fn parse_default_literal(value_tokens: &[TokenTree]) -> Result<DefaultLiteral, String> {
    let Some(token) = value_tokens.first() else {
        return Err("attribute `default = value(...)` requires a literal value".to_owned());
    };

    if let TokenTree::Ident(ident) = token {
        return match ident.to_string().as_str() {
            "null" => Ok(DefaultLiteral::Null),
            "true" => Ok(DefaultLiteral::Bool(true)),
            "false" => Ok(DefaultLiteral::Bool(false)),
            _ => Err(format!("unsupported default literal `{ident}`")),
        };
    }

    let mut negative = false;
    let token = if matches!(token, TokenTree::Punct(punct) if punct.as_char() == '-') {
        negative = true;
        value_tokens
            .get(1)
            .ok_or_else(|| "negative default value requires a numeric literal".to_owned())?
    } else {
        token
    };

    let TokenTree::Literal(literal_token) = token else {
        return Err("attribute `default = value(...)` requires a literal value".to_owned());
    };

    let literal = literal_token.to_string();
    if literal.starts_with('"') {
        if negative {
            return Err("string default values cannot be negative".to_owned());
        }
        return Ok(DefaultLiteral::Text(literal_string(literal_token)));
    }

    let literal = literal
        .trim_end_matches("i8")
        .trim_end_matches("i16")
        .trim_end_matches("i32")
        .trim_end_matches("i64")
        .trim_end_matches("i128")
        .trim_end_matches("isize")
        .trim_end_matches("u8")
        .trim_end_matches("u16")
        .trim_end_matches("u32")
        .trim_end_matches("u64")
        .trim_end_matches("u128")
        .trim_end_matches("usize")
        .trim_end_matches("f32")
        .trim_end_matches("f64");

    if literal.contains('.') {
        let mut value = literal
            .parse::<f64>()
            .map_err(|_| "float default value could not be parsed".to_owned())?;
        if negative {
            value = -value;
        }
        return Ok(DefaultLiteral::Float(value));
    }

    if negative {
        let value = literal
            .parse::<i128>()
            .map_err(|_| "integer default value could not be parsed".to_owned())?;
        Ok(DefaultLiteral::Int(-value))
    } else {
        if let Ok(value) = literal.parse::<i128>() {
            Ok(DefaultLiteral::Int(value))
        } else {
            let value = literal
                .parse::<u128>()
                .map_err(|_| "integer default value could not be parsed".to_owned())?;
            Ok(DefaultLiteral::UInt(value))
        }
    }
}

fn required_bool(name: &str, value_tokens: &[TokenTree]) -> Result<bool, String> {
    let Some(token) = value_tokens.first() else {
        return Err(format!("attribute `{name}` requires a boolean value"));
    };

    match token.to_string().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("attribute `{name}` requires a boolean value")),
    }
}

fn parse_references(value_tokens: &[TokenTree]) -> Result<ForeignKeyAttrs, String> {
    let Some(TokenTree::Group(group)) = value_tokens.first() else {
        return Err("references requires metadata like references(User::id)".to_owned());
    };

    let mut references = ForeignKeyAttrs::default();
    let tokens = group.stream().into_iter().collect::<Vec<_>>();
    let mut index = parse_reference_target(&tokens, &mut references)?;

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
            "on_delete" => references.on_delete = Some(value),
            "on_update" => references.on_update = Some(value),
            _ => return Err(format!("unsupported references option `{name}`")),
        }
    }

    if references.table.is_none() || references.column.is_none() {
        return Err("references requires a table field path like `User::id`".to_owned());
    }

    Ok(references)
}

fn parse_reference_target(
    tokens: &[TokenTree],
    references: &mut ForeignKeyAttrs,
) -> Result<usize, String> {
    let target_end = tokens
        .iter()
        .position(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ','))
        .unwrap_or(tokens.len());
    let target = &tokens[..target_end];

    let [
        TokenTree::Ident(table),
        TokenTree::Punct(first_colon),
        TokenTree::Punct(second_colon),
        TokenTree::Ident(column),
    ] = target
    else {
        return Err("references requires a table field path like `User::id`".to_owned());
    };

    if first_colon.as_char() != ':' || second_colon.as_char() != ':' {
        return Err("references requires a table field path like `User::id`".to_owned());
    }

    references.table = Some(table.clone());
    references.column = Some(column.clone());

    Ok(target_end)
}
