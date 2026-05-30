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
                <#types as ProjectionShape>::Exprs<'static>: Projectable,
            )*
        {
            type Exprs<'scope> = (
                #(
                    <<#types as ProjectionShape>::Exprs<'static> as Projectable>::Rebound<'scope>,
                )*
            );
            type Row = (
                #(<#types as ProjectionShape>::Row,)*
            );

            fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
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
            type Rebound<'scope> = (
                #(<#types as Projectable>::Rebound<'scope>,)*
            );

            fn project(&self) -> Vec<SelectColumn> {
                let mut columns = Vec::new();
                #(
                    columns.extend(
                        self.#fields.project().into_iter().map(|column| {
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
    }
}
