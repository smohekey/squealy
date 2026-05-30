use proc_macro::TokenStream;
use proc_macro2::{Ident, Literal, Span};

pub(crate) fn projection_shapes(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 2 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("tuple_projection_shapes! expects a maximum arity of at least 2");
            }
            .into();
        }
    };

    let impls = (2..=max_arity).map(tuple_projection_shape);

    quote::quote! {
        #(#impls)*
    }
    .into()
}

pub(crate) fn ir_lists(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("tuple_ir_lists! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let lists = (1..=max_arity).map(tuple_ir_list);
    let appends = (1..max_arity).map(tuple_ir_append);
    let concats = (1..=max_arity)
        .flat_map(|left| (0..=(max_arity - left)).map(move |right| tuple_ir_concat(left, right)));

    quote::quote! {
        #(#lists)*
        #(#appends)*
        #(#concats)*
    }
    .into()
}

pub(crate) fn source_spec_lists(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("tuple_source_spec_lists! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let impls = (1..=max_arity).map(tuple_source_spec_list);

    quote::quote! {
        #(#impls)*
    }
    .into()
}

pub(crate) fn pushes(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("tuple_pushes! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let impls = (1..max_arity).map(tuple_push);

    quote::quote! {
        #(#impls)*
    }
    .into()
}

pub(crate) fn lens(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("tuple_lens! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let impls = (1..=max_arity).map(tuple_len);

    quote::quote! {
        #(#impls)*
    }
    .into()
}

fn tuple_ir_list(arity: usize) -> proc_macro2::TokenStream {
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();
    let types = (0..arity).map(|_| quote::quote! { T }).collect::<Vec<_>>();

    quote::quote! {
        impl<T> IrList<T> for (#(#types,)*)
        {
            fn len(&self) -> usize {
                #arity
            }

            fn try_for_each<E>(
                &self,
                mut f: impl FnMut(&T) -> Result<(), E>,
            ) -> Result<(), E> {
                #(
                    f(&self.#fields)?;
                )*
                Ok(())
            }

            fn into_vec(self) -> Vec<T> {
                vec![
                    #(self.#fields,)*
                ]
            }
        }
    }
}

fn tuple_ir_append(arity: usize) -> proc_macro2::TokenStream {
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();
    let types = (0..arity).map(|_| quote::quote! { T }).collect::<Vec<_>>();
    let output = (0..=arity).map(|_| quote::quote! { T }).collect::<Vec<_>>();

    quote::quote! {
        impl<T> TupleAppend<T> for (#(#types,)*)
        {
            type Output = (#(#output,)*);

            fn append(self, value: T) -> Self::Output {
                (
                    #(self.#fields,)*
                    value,
                )
            }
        }
    }
}

fn tuple_ir_concat(left_arity: usize, right_arity: usize) -> proc_macro2::TokenStream {
    let left_fields = (0..left_arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();
    let right_fields = (0..right_arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();
    let left_types = (0..left_arity)
        .map(|_| quote::quote! { T })
        .collect::<Vec<_>>();
    let right_types = (0..right_arity)
        .map(|_| quote::quote! { T })
        .collect::<Vec<_>>();
    let output = (0..(left_arity + right_arity))
        .map(|_| quote::quote! { T })
        .collect::<Vec<_>>();

    quote::quote! {
        impl<T> TupleConcat<T, (#(#right_types,)*)> for (#(#left_types,)*)
        {
            type Output = (#(#output,)*);

            fn concat(self, rhs: (#(#right_types,)*)) -> Self::Output {
                (
                    #(self.#left_fields,)*
                    #(rhs.#right_fields,)*
                )
            }
        }
    }
}

fn tuple_source_spec_list(arity: usize) -> proc_macro2::TokenStream {
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();
    let types = (0..arity)
        .map(|index| Ident::new(&format!("Source{index}"), Span::call_site()))
        .collect::<Vec<_>>();

    quote::quote! {
        impl<#(#types),*> SourceSpecList for (#(#types,)*)
        where
            #(#types: SourceSpec,)*
        {
            fn into_sources(self) -> Vec<Source> {
                vec![
                    #(self.#fields.into_source(),)*
                ]
            }
        }
    }
}

fn tuple_len(arity: usize) -> proc_macro2::TokenStream {
    let types = (0..arity)
        .map(|index| Ident::new(&format!("T{index}"), Span::call_site()))
        .collect::<Vec<_>>();

    quote::quote! {
        impl<#(#types),*> TupleLen<#arity> for (#(#types,)*)
        {
        }
    }
}

fn tuple_push(arity: usize) -> proc_macro2::TokenStream {
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();
    let types = (0..arity)
        .map(|index| Ident::new(&format!("T{index}"), Span::call_site()))
        .collect::<Vec<_>>();

    quote::quote! {
        impl<Item, #(#types),*> TuplePush<Item> for (#(#types,)*)
        {
            type Output = (#(#types,)* Item,);

            fn push(self, value: Item) -> Self::Output {
                (
                    #(self.#fields,)*
                    value,
                )
            }
        }
    }
}

fn tuple_projection_shape(arity: usize) -> proc_macro2::TokenStream {
    let types = (0..arity)
        .map(|index| Ident::new(&format!("T{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let exprs = (0..arity)
        .map(|index| Ident::new(&format!("exprs{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();
    let prefixes = (0..arity)
        .map(|index| Literal::string(&format!("t{index}")))
        .collect::<Vec<_>>();

    quote::quote! {
        impl<#(#types),*> ProjectionShape for (#(#types,)*)
        where
            #(
                #types: ProjectionShape,
                <#types as ProjectionShape>::Row: Send,
                for<'any> <#types as ProjectionShape>::Exprs<'any>: Projectable,
            )*
        {
            type Exprs<'scope> = (
                #(
                    <#types as ProjectionShape>::Exprs<'scope>,
                )*
            );
            type ReboundExprs<'scope> = (
                #(
                    <<#types as ProjectionShape>::Exprs<'scope> as Projectable>::Rebound<'scope>,
                )*
            );
            type Row = (
                #(<#types as ProjectionShape>::Row,)*
            );

            fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
                _ = alias;
                (
                    #(#types::exprs(alias),)*
                )
            }

            fn rebound_exprs<'scope>(alias: &str) -> Self::ReboundExprs<'scope> {
                #(
                    let #exprs = #types::exprs(alias);
                )*

                (
                    #(#exprs.re_alias_with_prefix(alias, #prefixes),)*
                )
            }
        }

        impl<#(#types),*> Projectable for (#(#types,)*)
        where
            #(#types: Projectable,)*
        {
            type Columns = Vec<SelectColumn>;
            type Rebound<'scope> = (
                #(<#types as Projectable>::Rebound<'scope>,)*
            );

            fn project(&self) -> Self::Columns {
                let mut columns = Vec::new();
                #(
                    columns.extend(
                        self.#fields.project().into_vec().into_iter().map(|column| {
                            SelectColumn::new(column.expr, prefix_alias(#prefixes, &column.alias))
                        }),
                    );
                )*
                columns
            }

            fn re_alias<'scope>(&self, alias: &str) -> Self::Rebound<'scope> {
                (
                    #(self.#fields.re_alias_with_prefix(alias, #prefixes),)*
                )
            }

            fn re_alias_with_prefix<'scope>(
                &self,
                alias: &str,
                prefix: &str,
            ) -> Self::Rebound<'scope> {
                (
                    #(
                        self.#fields
                            .re_alias_with_prefix(alias, &format!("{prefix}_{}", #prefixes)),
                    )*
                )
            }
        }

        impl<'scope, #(#types),*> ReturningProjection<'scope> for (#(#types,)*)
        where
            #(#types: ReturningProjection<'scope>,)*
        {
            type Shape = (
                #(<#types as ReturningProjection<'scope>>::Shape,)*
            );
        }

        impl<Backend, #(#types),*> Decode<Backend> for (#(#types,)*)
        where
            Backend: ::squealy::Backend,
            #(#types: Decode<Backend>,)*
        {
            fn decode(
                row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
            ) -> Result<Self, <Backend as ::squealy::Backend>::Error> {
                Ok((
                    #(::squealy::RowReader::read::<#types>(row)?,)*
                ))
            }
        }
    }
}
