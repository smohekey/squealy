use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};
use proc_macro2::{Literal, Span, TokenStream as TokenStream2};

use crate::common::{
    bool_tokens, compile_error, foreign_key_ident, generated_ident, is_attribute_start,
    literal_string, matches_ident, option_literal, required_literal, to_pascal, to_snake_plural,
};

pub(crate) fn derive(input: TokenStream) -> TokenStream {
    match table_struct(input) {
        Ok(table) => table.expand(),
        Err(message) => compile_error(&message),
    }
}

struct TableStruct {
    ident: Ident,
    fields: Vec<Field>,
    indexes: Vec<IndexAttrs>,
    schema: Option<proc_macro2::TokenStream>,
    has_scope_and_mode: bool,
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

#[derive(Default)]
struct TableAttrs {
    indexes: Vec<IndexAttrs>,
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
    default: Option<String>,
    db_type: Option<String>,
    check: Option<String>,
    references: Option<ForeignKeyAttrs>,
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
        if !self.has_scope_and_mode {
            return compile_error(
                "Table currently requires structs shaped like `Type<'scope, C: ColumnMode = ColumnExpr>`",
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
        let schema = self
            .schema
            .clone()
            .unwrap_or_else(|| quote::quote! { ::squealy::DefaultSchema });
        let insert_builder = self.insert_builder_tokens(&ident, &field_literals);
        let update_builder = self.update_builder_tokens(&ident, &field_literals);
        let insert_builder_defs = insert_builder.definitions;
        let insert_table_impl = insert_builder.table_impl;
        let update_builder_defs = update_builder.definitions;
        let update_table_impl = update_builder.table_impl;

        quote::quote! {
            #(#foreign_key_defs)*
            #(#column_defs)*
            #(#index_defs)*

            #(
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                pub enum #expr_kind_idents {}

                impl ::squealy::ExprKind for #expr_kind_idents {
                    type Value = #field_value_tys;
                }

                impl ::squealy::ProjectionShape for #expr_kind_idents {
                    type Exprs<'scope> = ::squealy::ColumnRef<'scope, #expr_kind_idents>;
                    type ReboundExprs<'scope> = ::squealy::Expr<'scope, #expr_kind_idents>;
                    type Row = #field_value_tys;

                    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
                        ::squealy::ColumnRef::column(alias, #field_literals)
                    }

                    fn rebound_exprs<'scope>(alias: &str) -> Self::ReboundExprs<'scope> {
                        ::squealy::Expr::column(alias, #field_literals)
                    }
                }
            )*

            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            struct #exprs_ident <'scope> {
                #( pub #fields: ::squealy::ColumnRef<'scope, #expr_kind_idents>, )*
            }

            #[derive(Clone, Debug, PartialEq)]
            struct #rebound_exprs_ident <'scope> {
                #( pub #fields: ::squealy::Expr<'scope, #expr_kind_idents>, )*
            }

            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            struct #nullable_exprs_ident <'scope> {
                #( pub #fields: ::squealy::ColumnRef<'scope, ::squealy::Nullable<#expr_kind_idents>>, )*
            }

            #[derive(Clone, Debug, PartialEq)]
            struct #nullable_rebound_exprs_ident <'scope> {
                #( pub #fields: ::squealy::Expr<'scope, ::squealy::Nullable<#expr_kind_idents>>, )*
            }

            #insert_builder_defs
            #update_builder_defs

            static #columns_static: [&'static dyn ::squealy::Column; #columns_len] = [#( &#column_idents, )*];
            static #indexes_static: [&'static dyn ::squealy::Index; #indexes_len] = [#( &#index_idents, )*];

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

                fn column_names() -> Self::WithColumn<'static, ::squealy::ColumnName> {
                    #ident { #( #fields: #field_literals, )* }
                }

                fn column_exprs_from<'next_scope>(
                    alias: &str,
                    columns: &Self::WithColumn<'static, ::squealy::ColumnName>,
                ) -> Self::Exprs<'next_scope> {
                    #exprs_ident { #( #fields: ::squealy::ColumnRef::column(alias, columns.#fields), )* }
                }

                fn nullable_column_exprs_from<'next_scope>(
                    alias: &str,
                    columns: &Self::WithColumn<'static, ::squealy::ColumnName>,
                ) -> Self::NullableExprs<'next_scope> {
                    #nullable_exprs_ident { #( #fields: ::squealy::ColumnRef::column(alias, columns.#fields), )* }
                }
            }

            #insert_table_impl
            #update_table_impl

            impl<'scope> ::squealy::Projectable for #exprs_ident <'scope> {
                type Rebound<'next_scope> = #rebound_exprs_ident <'next_scope>;

                fn project(&self) -> ::std::vec::Vec<::squealy::SelectColumn> {
                    ::std::vec![
                        #(
                            ::squealy::SelectColumn::new(
                                self.#fields.node(),
                                #field_literals,
                            ),
                        )*
                    ]
                }

                fn re_alias<'next_scope>(&self, alias: &str) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: &str,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, &::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #exprs_ident <'scope> {
                type Shape = #ident <'static, ::squealy::ColumnExpr>;
            }

            impl<'scope> ::squealy::Projectable for #rebound_exprs_ident <'scope> {
                type Rebound<'next_scope> = #rebound_exprs_ident <'next_scope>;

                fn project(&self) -> ::std::vec::Vec<::squealy::SelectColumn> {
                    ::std::vec![
                        #(
                            ::squealy::SelectColumn::new(
                                self.#fields.node().clone(),
                                #field_literals,
                            ),
                        )*
                    ]
                }

                fn re_alias<'next_scope>(&self, alias: &str) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: &str,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, &::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #rebound_exprs_ident <'scope> {
                type Shape = #ident <'static, ::squealy::ColumnExpr>;
            }

            impl<'scope> ::squealy::Projectable for #nullable_exprs_ident <'scope> {
                type Rebound<'next_scope> = #nullable_rebound_exprs_ident <'next_scope>;

                fn project(&self) -> ::std::vec::Vec<::squealy::SelectColumn> {
                    ::std::vec![
                        #(
                            ::squealy::SelectColumn::new(
                                self.#fields.node(),
                                #field_literals,
                            ),
                        )*
                    ]
                }

                fn re_alias<'next_scope>(&self, alias: &str) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: &str,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, &::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #nullable_exprs_ident <'scope> {
                type Shape = ::squealy::Maybe<#ident <'static, ::squealy::ColumnExpr>>;
            }

            impl<'scope> ::squealy::Projectable for #nullable_rebound_exprs_ident <'scope> {
                type Rebound<'next_scope> = #nullable_rebound_exprs_ident <'next_scope>;

                fn project(&self) -> ::std::vec::Vec<::squealy::SelectColumn> {
                    ::std::vec![
                        #(
                            ::squealy::SelectColumn::new(
                                self.#fields.node().clone(),
                                #field_literals,
                            ),
                        )*
                    ]
                }

                fn re_alias<'next_scope>(&self, alias: &str) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: &str,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, &::std::format!("{prefix}_{}", #field_literals)), )*
                    }
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
    fn insert_builder_tokens(
        &self,
        table_ident: &proc_macro2::Ident,
        field_literals: &[Literal],
    ) -> BuilderExpansion {
        let builder_ident = generated_ident(table_ident, "insert", "Builder");
        let fields = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.insertable())
            .collect::<Vec<_>>();
        let state_idents = fields
            .iter()
            .map(|(_, field)| generated_ident(table_ident, &field.ident.to_string(), "InsertState"))
            .collect::<Vec<_>>();
        let missing_idents = fields
            .iter()
            .map(|(_, field)| {
                generated_ident(table_ident, &field.ident.to_string(), "InsertMissing")
            })
            .collect::<Vec<_>>();
        let set_idents = fields
            .iter()
            .map(|(_, field)| generated_ident(table_ident, &field.ident.to_string(), "InsertSet"))
            .collect::<Vec<_>>();
        let state_defaults = state_idents
            .iter()
            .zip(missing_idents.iter())
            .map(|(state, missing)| quote::quote! { #state = #missing })
            .collect::<Vec<_>>();
        let initial_states = missing_idents.iter().collect::<Vec<_>>();
        let execute_states = fields
            .iter()
            .zip(state_idents.iter())
            .zip(set_idents.iter())
            .map(|(((_, field), state), set)| {
                if field.required_insert() {
                    quote::quote! { #set }
                } else {
                    quote::quote! { #state }
                }
            })
            .collect::<Vec<_>>();
        let execute_state_params = fields
            .iter()
            .zip(state_idents.iter())
            .filter_map(|((_, field), state)| (!field.required_insert()).then_some(state))
            .collect::<Vec<_>>();
        let state_tuple = if state_idents.is_empty() {
            quote::quote! { () }
        } else {
            quote::quote! { (#(#state_idents,)*) }
        };
        let setters = fields
            .iter()
            .enumerate()
            .map(|(setter_index, (field_index, field))| {
                let field_ident =
                    proc_macro2::Ident::new(&field.ident.to_string(), Span::call_site());
                let field_literal = &field_literals[*field_index];
                let field_ty = &field.value_ty;
                let missing = &missing_idents[setter_index];
                let set = &set_idents[setter_index];
                let impl_state_params = state_idents
                    .iter()
                    .enumerate()
                    .filter_map(|(index, state)| (index != setter_index).then_some(state))
                    .collect::<Vec<_>>();
                let self_states = state_idents
                    .iter()
                    .enumerate()
                    .map(|(index, state)| {
                        if index == setter_index {
                            quote::quote! { #missing }
                        } else {
                            quote::quote! { #state }
                        }
                    })
                    .collect::<Vec<_>>();
                let return_states = state_idents
                    .iter()
                    .enumerate()
                    .map(|(index, state)| {
                        if index == setter_index {
                            quote::quote! { #set }
                        } else {
                            quote::quote! { #state }
                        }
                    })
                    .collect::<Vec<_>>();

                quote::quote! {
                    impl<'conn, Conn, #(#impl_state_params),*> #builder_ident <'conn, Conn, #(#self_states),*>
                    where
                        Conn: ::squealy::Connection + 'conn,
                    {
                        pub fn #field_ident(
                            mut self,
                            value: impl ::std::convert::Into<#field_ty>,
                        ) -> #builder_ident <'conn, Conn, #(#return_states),*> {
                            self.columns.push(::squealy::InsertColumn::new(
                                #field_literal,
                                ::squealy::IntoBindValue::into_bind_value(value.into()),
                            ));
                            #builder_ident {
                                connection: self.connection,
                                columns: self.columns,
                                _state: ::std::marker::PhantomData,
                            }
                        }
                    }
                }
            })
            .collect::<Vec<_>>();

        let definitions = quote::quote! {
            #(
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                struct #missing_idents;
            )*

            #(
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                struct #set_idents;
            )*

            #[derive(Clone, Debug, PartialEq)]
            struct #builder_ident <'conn, Conn: ::squealy::Connection + 'conn, #(#state_defaults),*> {
                connection: &'conn Conn,
                columns: ::std::vec::Vec<::squealy::InsertColumn>,
                _state: ::std::marker::PhantomData<#state_tuple>,
            }

            impl<'conn, Conn> #builder_ident <'conn, Conn, #(#initial_states),*>
            where
                Conn: ::squealy::Connection + 'conn,
            {
                fn new(connection: &'conn Conn) -> Self {
                    Self {
                        connection,
                        columns: ::std::vec::Vec::new(),
                        _state: ::std::marker::PhantomData,
                    }
                }
            }

            #(#setters)*

            impl<'conn, Conn, #(#execute_state_params),*> #builder_ident <'conn, Conn, #(#execute_states),*>
            where
                Conn: ::squealy::Connection + 'conn,
            {
                const ALIAS: &'static str = "q0_0";

                pub fn execute(
                    self,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<u64, <Conn as ::squealy::Connection>::Error>,
                > + 'conn {
                    let query = ::squealy::Connection::insert_query::<#table_ident <'static, ::squealy::ColumnExpr>>(
                        self.connection,
                        self.columns,
                    );
                    async move {
                        ::squealy::InsertQuery::execute(&query).await
                    }
                }

                pub fn returning<P>(
                    self,
                    projection: impl ::std::ops::FnOnce(
                        <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
                    ) -> P,
                ) -> <Conn as ::squealy::Connection>::Insert<'conn, #table_ident <'static, ::squealy::ColumnExpr>, <P as ::squealy::ReturningProjection<'static>>::Shape>
                where
                    P: ::squealy::ReturningProjection<'static>,
                {
                    let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
                    let projection = projection(table);
                    ::squealy::Connection::insert_returning_query::<#table_ident <'static, ::squealy::ColumnExpr>, <P as ::squealy::ReturningProjection<'static>>::Shape>(
                        self.connection,
                        self.columns,
                        ::squealy::Projectable::project(&projection),
                    )
                }
            }
        };

        let table_impl = quote::quote! {
            impl ::squealy::InsertableTable for #table_ident <'static, ::squealy::ColumnExpr> {
                type InsertBuilder<'conn, Conn> = #builder_ident <'conn, Conn>
                where
                    Conn: ::squealy::Connection + 'conn;

                fn insert_builder<'conn, Conn>(
                    connection: &'conn Conn,
                ) -> Self::InsertBuilder<'conn, Conn>
                where
                    Conn: ::squealy::Connection + 'conn,
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

    fn update_builder_tokens(
        &self,
        table_ident: &proc_macro2::Ident,
        field_literals: &[Literal],
    ) -> BuilderExpansion {
        let builder_ident = generated_ident(table_ident, "update", "Builder");
        let ready_ident = generated_ident(table_ident, "update", "Ready");
        let fields = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.updateable())
            .collect::<Vec<_>>();
        let state_idents = fields
            .iter()
            .map(|(_, field)| generated_ident(table_ident, &field.ident.to_string(), "UpdateState"))
            .collect::<Vec<_>>();
        let missing_idents = fields
            .iter()
            .map(|(_, field)| {
                generated_ident(table_ident, &field.ident.to_string(), "UpdateMissing")
            })
            .collect::<Vec<_>>();
        let set_idents = fields
            .iter()
            .map(|(_, field)| generated_ident(table_ident, &field.ident.to_string(), "UpdateSet"))
            .collect::<Vec<_>>();
        let state_defaults = state_idents
            .iter()
            .zip(missing_idents.iter())
            .map(|(state, missing)| quote::quote! { #state = #missing })
            .collect::<Vec<_>>();
        let state_params = state_idents
            .iter()
            .map(|state| quote::quote! { #state })
            .collect::<Vec<_>>();
        let initial_states = missing_idents.iter().collect::<Vec<_>>();
        let state_tuple = if state_idents.is_empty() {
            quote::quote! { () }
        } else {
            quote::quote! { (#(#state_idents,)*) }
        };
        let ready_impls = (1usize..(1usize << fields.len()))
            .map(|mask| {
                let states = missing_idents
                    .iter()
                    .zip(set_idents.iter())
                    .enumerate()
                    .map(|(index, (missing, set))| {
                        if (mask & (1usize << index)) == 0 {
                            quote::quote! { #missing }
                        } else {
                            quote::quote! { #set }
                        }
                    })
                    .collect::<Vec<_>>();
                quote::quote! {
                    impl #ready_ident for (#(#states,)*) {}
                }
            })
            .collect::<Vec<_>>();
        let setters = fields
            .iter()
            .enumerate()
            .map(|(setter_index, (field_index, field))| {
                let field_ident =
                    proc_macro2::Ident::new(&field.ident.to_string(), Span::call_site());
                let field_literal = &field_literals[*field_index];
                let field_ty = &field.value_ty;
                let missing = &missing_idents[setter_index];
                let set = &set_idents[setter_index];
                let impl_state_params = state_idents
                    .iter()
                    .enumerate()
                    .filter_map(|(index, state)| (index != setter_index).then_some(state))
                    .collect::<Vec<_>>();
                let self_states = state_idents
                    .iter()
                    .enumerate()
                    .map(|(index, state)| {
                        if index == setter_index {
                            quote::quote! { #missing }
                        } else {
                            quote::quote! { #state }
                        }
                    })
                    .collect::<Vec<_>>();
                let return_states = state_idents
                    .iter()
                    .enumerate()
                    .map(|(index, state)| {
                        if index == setter_index {
                            quote::quote! { #set }
                        } else {
                            quote::quote! { #state }
                        }
                    })
                    .collect::<Vec<_>>();

                quote::quote! {
                    impl<'conn, Conn, FilterState, #(#impl_state_params),*> #builder_ident <'conn, Conn, FilterState, #(#self_states),*>
                    where
                        Conn: ::squealy::Connection + 'conn,
                    {
                        pub fn #field_ident(
                            mut self,
                            value: impl ::std::convert::Into<#field_ty>,
                        ) -> #builder_ident <'conn, Conn, FilterState, #(#return_states),*> {
                            self.columns.push(::squealy::UpdateColumn::new(
                                #field_literal,
                                ::squealy::IntoBindValue::into_bind_value(value.into()),
                            ));
                            #builder_ident {
                                connection: self.connection,
                                columns: self.columns,
                                filters: self.filters,
                                _state: ::std::marker::PhantomData,
                            }
                        }
                    }
                }
            })
            .collect::<Vec<_>>();

        let definitions = quote::quote! {
            #(
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                struct #missing_idents;
            )*

            #(
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                struct #set_idents;
            )*

            trait #ready_ident {}
            #(#ready_impls)*

            #[derive(Clone, Debug, PartialEq)]
            struct #builder_ident <'conn, Conn: ::squealy::Connection + 'conn, FilterState = ::squealy::MutationUnfiltered, #(#state_defaults),*> {
                connection: &'conn Conn,
                columns: ::std::vec::Vec<::squealy::UpdateColumn>,
                filters: ::std::vec::Vec<::squealy::Filter>,
                _state: ::std::marker::PhantomData<(FilterState, #state_tuple)>,
            }

            impl<'conn, Conn> #builder_ident <'conn, Conn, ::squealy::MutationUnfiltered, #(#initial_states),*>
            where
                Conn: ::squealy::Connection + 'conn,
            {
                fn new(connection: &'conn Conn) -> Self {
                    Self {
                        connection,
                        columns: ::std::vec::Vec::new(),
                        filters: ::std::vec::Vec::new(),
                        _state: ::std::marker::PhantomData,
                    }
                }
            }

            impl<'conn, Conn, FilterState, #(#state_idents),*> #builder_ident <'conn, Conn, FilterState, #(#state_idents),*>
            where
                Conn: ::squealy::Connection + 'conn,
            {
                const ALIAS: &'static str = "q0_0";

                pub fn where_(
                    mut self,
                    predicate: impl ::std::ops::FnOnce(
                        &<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
                    ) -> ::squealy::Predicate<'static>,
                ) -> #builder_ident <'conn, Conn, ::squealy::MutationFiltered, #(#state_params),*> {
                    let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
                    let predicate = predicate(&table);
                    self.filters.push(::squealy::Filter::new(predicate.node().clone()));
                    #builder_ident {
                        connection: self.connection,
                        columns: self.columns,
                        filters: self.filters,
                        _state: ::std::marker::PhantomData,
                    }
                }

                pub fn all(self) -> #builder_ident <'conn, Conn, ::squealy::MutationFiltered, #(#state_params),*> {
                    #builder_ident {
                        connection: self.connection,
                        columns: self.columns,
                        filters: self.filters,
                        _state: ::std::marker::PhantomData,
                    }
                }
            }

            #(#setters)*

            impl<'conn, Conn, #(#state_idents),*> #builder_ident <'conn, Conn, ::squealy::MutationFiltered, #(#state_idents),*>
            where
                Conn: ::squealy::Connection + 'conn,
                #state_tuple: #ready_ident,
            {
                pub fn execute(
                    self,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<u64, <Conn as ::squealy::Connection>::Error>,
                > + 'conn {
                    let query = ::squealy::Connection::update_query::<#table_ident <'static, ::squealy::ColumnExpr>>(
                        self.connection,
                        Self::ALIAS.to_owned(),
                        self.columns,
                        self.filters,
                    );
                    async move {
                        ::squealy::UpdateQuery::execute(&query).await
                    }
                }

                pub fn returning<P>(
                    self,
                    projection: impl ::std::ops::FnOnce(
                        <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
                    ) -> P,
                ) -> <Conn as ::squealy::Connection>::Update<'conn, #table_ident <'static, ::squealy::ColumnExpr>, <P as ::squealy::ReturningProjection<'static>>::Shape>
                where
                    P: ::squealy::ReturningProjection<'static>,
                {
                    let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
                    let projection = projection(table);
                    ::squealy::Connection::update_returning_query::<#table_ident <'static, ::squealy::ColumnExpr>, <P as ::squealy::ReturningProjection<'static>>::Shape>(
                        self.connection,
                        Self::ALIAS.to_owned(),
                        self.columns,
                        self.filters,
                        ::squealy::Projectable::project(&projection),
                    )
                }
            }
        };

        let table_impl = quote::quote! {
            impl ::squealy::UpdateableTable for #table_ident <'static, ::squealy::ColumnExpr> {
                type UpdateBuilder<'conn, Conn> = #builder_ident <'conn, Conn>
                where
                    Conn: ::squealy::Connection + 'conn;

                fn update_builder<'conn, Conn>(
                    connection: &'conn Conn,
                ) -> Self::UpdateBuilder<'conn, Conn>
                where
                    Conn: ::squealy::Connection + 'conn,
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
        let default = option_literal(self.attrs.default.as_deref());
        let db_type = option_literal(self.attrs.db_type.as_deref());
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
                fn default(&self) -> Option<&'static str> { #default }
                fn db_type(&self) -> Option<&'static str> { #db_type }
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
    let table_attrs = table_attributes(&tokens[..struct_index])?;
    validate_index_columns(&table_attrs.indexes, &fields)?;

    Ok(TableStruct {
        ident,
        fields,
        indexes: table_attrs.indexes,
        schema: table_attrs.schema,
        has_scope_and_mode,
    })
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
    let tokens = group.stream().into_iter().collect::<Vec<_>>();
    let mut index = 0;

    while index < tokens.len() {
        let token = tokens[index].clone();
        if is_attribute_start(&token) {
            let Some(TokenTree::Group(attr)) = tokens.get(index + 1) else {
                return Err("Table field attribute is missing its bracketed body".to_owned());
            };
            apply_attribute(&attr, &mut pending_attrs)?;
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
            value_ty: column_value_type(&type_tokens)?,
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

fn column_value_type(type_tokens: &[TokenTree]) -> Result<proc_macro2::TokenStream, String> {
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
        "default" => attrs.default = Some(required_literal(name, value_tokens)?),
        "db_type" => attrs.db_type = Some(required_literal(name, value_tokens)?),
        "check" => attrs.check = Some(required_literal(name, value_tokens)?),
        "column_name" | "name" => attrs.column_name = Some(required_literal(name, value_tokens)?),
        "references" => attrs.references = Some(parse_references(value_tokens)?),
        _ => return Err(format!("unsupported Table field attribute `{name}`")),
    }

    Ok(())
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
