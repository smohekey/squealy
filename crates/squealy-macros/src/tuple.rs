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

pub(crate) fn fixed_lists(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("tuple_fixed_lists! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let lists = (1..=max_arity).map(tuple_fixed_list);
    let appends = (1..max_arity).map(tuple_fixed_append);
    let concats = (1..=max_arity).flat_map(|left| {
        (0..=(max_arity - left)).map(move |right| tuple_fixed_concat(left, right))
    });

    quote::quote! {
        #(#lists)*
        #(#appends)*
        #(#concats)*
    }
    .into()
}

pub(crate) fn hlist_tuples(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("hlist_tuples! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let impls = (1..=max_arity).map(hlist_tuple);

    quote::quote! {
        #(#impls)*
    }
    .into()
}

pub(crate) fn prepared_param_values(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("prepared_param_values! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let impls = (1..=max_arity).map(prepared_param_value);

    quote::quote! {
        #(#impls)*
    }
    .into()
}

pub(crate) fn insert_column_values(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("insert_column_values! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let impls = (1..=max_arity).map(insert_column_value);

    quote::quote! {
        #(#impls)*
    }
    .into()
}

pub(crate) fn update_column_values(input: TokenStream) -> TokenStream {
    let max_arity = match input.to_string().trim().parse::<usize>() {
        Ok(max_arity) if max_arity >= 1 => max_arity,
        _ => {
            return quote::quote! {
                compile_error!("update_column_values! expects a maximum arity of at least 1");
            }
            .into();
        }
    };

    let impls = (1..=max_arity).map(update_column_value);

    quote::quote! {
        #(#impls)*
    }
    .into()
}

fn tuple_fixed_list(arity: usize) -> proc_macro2::TokenStream {
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();
    let types = (0..arity).map(|_| quote::quote! { T }).collect::<Vec<_>>();
    let output_types = (0..arity).map(|_| quote::quote! { U }).collect::<Vec<_>>();

    quote::quote! {
        impl<T> FixedList<T> for (#(#types,)*)
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
        }

        impl<T, U> MapFixedList<T, U> for (#(#types,)*)
        {
            type Output = (#(#output_types,)*);

            fn map_list(self, mut f: impl FnMut(T) -> U) -> Self::Output {
                (
                    #(f(self.#fields),)*
                )
            }
        }
    }
}

fn tuple_fixed_append(arity: usize) -> proc_macro2::TokenStream {
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

fn tuple_fixed_concat(left_arity: usize, right_arity: usize) -> proc_macro2::TokenStream {
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

fn hlist_tuple(arity: usize) -> proc_macro2::TokenStream {
    let types = (0..arity)
        .map(|index| Ident::new(&format!("T{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let values = (0..arity)
        .map(|index| Ident::new(&format!("value{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let tails = (0..arity)
        .map(|index| Ident::new(&format!("tail{index}"), Span::call_site()))
        .collect::<Vec<_>>();

    let hlist_type = types.iter().rev().fold(quote::quote! { HNil }, |tail, ty| {
        quote::quote! { HCons<#ty, #tail> }
    });

    let mut destructures = Vec::new();
    for index in 0..arity {
        let input = if index == 0 {
            quote::quote! { self }
        } else {
            let previous_tail = &tails[index - 1];
            quote::quote! { #previous_tail }
        };
        let value = &values[index];
        let tail = &tails[index];
        destructures.push(quote::quote! {
            let HCons {
                head: #value,
                tail: #tail,
            } = #input;
        });
    }

    quote::quote! {
        impl<#(#types),*> ToTuple for #hlist_type
        {
            type Tuple = (#(#types,)*);

            fn to_tuple(self) -> Self::Tuple {
                #(#destructures)*
                (#(#values,)*)
            }
        }
    }
}

fn insert_column_value(arity: usize) -> proc_macro2::TokenStream {
    let columns = (0..arity)
        .map(|index| Ident::new(&format!("K{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let values = (0..arity)
        .map(|index| Ident::new(&format!("V{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();

    let assignment_types = columns
        .iter()
        .zip(values.iter())
        .map(|(column, value)| {
            quote::quote! {
                crate::InsertAssignment<
                    #column,
                    <<#column as crate::ColumnKey>::Nullability as crate::IntoInsertColumnValue<
                        #column,
                        #value
                    >>::AssignmentValue
                >
            }
        })
        .collect::<Vec<_>>();

    let assignments =
        assignment_types
            .iter()
            .rev()
            .fold(quote::quote! { crate::HNil }, |tail, assignment| {
                quote::quote! {
                    crate::HCons<#assignment, #tail>
                }
            });

    let assignment_tails = (0..arity)
        .map(|index| {
            assignment_types[(index + 1)..].iter().rev().fold(
                quote::quote! { crate::HNil },
                |tail, assignment| quote::quote! { crate::HCons<#assignment, #tail> },
            )
        })
        .collect::<Vec<_>>();

    let assignment_tail_bounds = assignment_tails
        .iter()
        .map(|tail| quote::quote! { #tail: crate::InsertAssignments, })
        .collect::<Vec<_>>();

    let append_bounds = columns
        .iter()
        .zip(values.iter())
        .zip(assignment_tails.iter())
        .map(|((column, value), tail)| {
            quote::quote! {
                <<<#column as crate::ColumnKey>::Nullability as crate::IntoInsertColumnValue<
                    #column,
                    #value
                >>::AssignmentValue as crate::AssignmentValueNode>::Params:
                    crate::HAppend<<#tail as crate::InsertAssignments>::Params>,
            }
        })
        .collect::<Vec<_>>();

    quote::quote! {
        impl<S, #(#columns,)* #(#values,)*> crate::InsertColumnValues<S, (#(#values,)*)>
            for (#(#columns,)*)
        where
            S: crate::InsertableTable,
            #(
                #columns: crate::InsertColumnKey<Table = S>,
                <#columns as crate::ColumnKey>::Nullability:
                    crate::IntoInsertColumnValue<#columns, #values>,
            )*
            #assignments: crate::InsertAssignments,
            #(#assignment_tail_bounds)*
            #(#append_bounds)*
        {
            type Assignments = #assignments;

            fn into_insert_assignments(values: (#(#values,)*)) -> Self::Assignments {
                crate::HNil
                    #(
                        .push_back(crate::InsertAssignment::<#columns, _>::new(
                            <<#columns as crate::ColumnKey>::Nullability as crate::IntoInsertColumnValue<
                                #columns,
                                #values
                            >>::into_insert_column_value(values.#fields)
                        ))
                    )*
            }
        }
    }
}

fn update_column_value(arity: usize) -> proc_macro2::TokenStream {
    let columns = (0..arity)
        .map(|index| Ident::new(&format!("K{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let values = (0..arity)
        .map(|index| Ident::new(&format!("V{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();

    let assignment_types = columns
        .iter()
        .zip(values.iter())
        .map(|(column, value)| {
            quote::quote! {
                crate::UpdateAssignment<
                    #column,
                    <<#column as crate::ColumnKey>::Nullability as crate::IntoUpdateColumnValue<
                        #column,
                        #value
                    >>::AssignmentValue
                >
            }
        })
        .collect::<Vec<_>>();

    let assignments =
        assignment_types
            .iter()
            .rev()
            .fold(quote::quote! { crate::HNil }, |tail, assignment| {
                quote::quote! {
                    crate::HCons<#assignment, #tail>
                }
            });

    let assignment_tails = (0..arity)
        .map(|index| {
            assignment_types[(index + 1)..].iter().rev().fold(
                quote::quote! { crate::HNil },
                |tail, assignment| quote::quote! { crate::HCons<#assignment, #tail> },
            )
        })
        .collect::<Vec<_>>();

    let assignment_tail_bounds = assignment_tails
        .iter()
        .map(|tail| quote::quote! { #tail: crate::UpdateAssignments, })
        .collect::<Vec<_>>();

    let append_bounds = columns
        .iter()
        .zip(values.iter())
        .zip(assignment_tails.iter())
        .map(|((column, value), tail)| {
            quote::quote! {
                <<<#column as crate::ColumnKey>::Nullability as crate::IntoUpdateColumnValue<
                    #column,
                    #value
                >>::AssignmentValue as crate::AssignmentValueNode>::Params:
                    crate::HAppend<<#tail as crate::UpdateAssignments>::Params>,
            }
        })
        .collect::<Vec<_>>();

    quote::quote! {
        impl<S, #(#columns,)* #(#values,)*> crate::UpdateColumnValues<S, (#(#values,)*)>
            for (#(#columns,)*)
        where
            S: crate::UpdateableTable,
            #(
                #columns: crate::UpdateColumnKey<Table = S>,
                <#columns as crate::ColumnKey>::Nullability:
                    crate::IntoUpdateColumnValue<#columns, #values>,
            )*
            #assignments: crate::UpdateAssignments,
            #(#assignment_tail_bounds)*
            #(#append_bounds)*
        {
            type Assignments = #assignments;

            fn into_update_assignments(values: (#(#values,)*)) -> Self::Assignments {
                crate::HNil
                    #(
                        .push_back(crate::UpdateAssignment::<#columns, _>::new(
                            <<#columns as crate::ColumnKey>::Nullability as crate::IntoUpdateColumnValue<
                                #columns,
                                #values
                            >>::into_update_column_value(values.#fields)
                        ))
                    )*
            }
        }
    }
}

fn prepared_param_value(arity: usize) -> proc_macro2::TokenStream {
    let types = (0..arity)
        .map(|index| Ident::new(&format!("T{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let values = (0..arity)
        .map(|index| Ident::new(&format!("V{index}"), Span::call_site()))
        .collect::<Vec<_>>();
    let fields = (0..arity)
        .map(Literal::usize_unsuffixed)
        .collect::<Vec<_>>();

    let hlist_type = types.iter().rev().fold(quote::quote! { HNil }, |tail, ty| {
        quote::quote! { HCons<#ty, #tail> }
    });

    quote::quote! {
        impl<B, #(#types,)* #(#values),*> PreparedParamValues<#hlist_type, B> for (#(#values,)*)
        where
            B: crate::Backend,
            #(#values: crate::IntoPreparedParam<#types> + Clone,)*
            #(#types: crate::Encode<B>,)*
        {
            fn write_params(
                &self,
                writer: &mut <B as crate::Backend>::ParamWriter<'_>,
            ) -> ::std::result::Result<(), <B as crate::Backend>::Error> {
                #(
                    crate::ParamWriter::write(writer, &self.#fields.clone().into_prepared_param())?;
                )*
                Ok(())
            }

            fn write_param_at(
                &self,
                index: usize,
                writer: &mut <B as crate::Backend>::ParamWriter<'_>,
            ) -> ::std::result::Result<bool, <B as crate::Backend>::Error> {
                match index {
                    #(
                        #fields => {
                            crate::ParamWriter::write(writer, &self.#fields.clone().into_prepared_param())?;
                            Ok(true)
                        }
                    )*
                    _ => Ok(false),
                }
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
    let exprs_tuple = quote::quote! {
        (#(<#types as ProjectionShape>::Exprs<'scope>,)*)
    };
    let returning_shape_tuple = quote::quote! {
        (#(<#types as ReturningProjection<'scope>>::Shape,)*)
    };
    // The projection's aggregate class is the first element's; every element must agree, so a
    // tuple mixing a scalar column and an aggregate has no `ProjectionClass` impl.
    let first_type = &types[0];
    let rest_types = &types[1..];
    // Each element's projection params, concatenated left-to-right (render order), so an embedded
    // subquery's `Params` accounts for a runtime `param` in any projected expression.
    let projection_param_types = types
        .iter()
        .map(|ty| quote::quote! { <#ty as crate::ProjectionParams>::Params })
        .collect::<Vec<_>>();
    let projection_params_folded = projection_param_types
        .iter()
        .rev()
        .cloned()
        .reduce(|tail, head| quote::quote! { <#head as crate::HAppend<#tail>>::Output })
        .unwrap_or_else(|| quote::quote! { crate::HNil });
    let projection_params_append_bounds = (0..arity)
        .map(|index| {
            match projection_param_types[(index + 1)..]
                .iter()
                .rev()
                .cloned()
                .reduce(|tail, head| quote::quote! { <#head as crate::HAppend<#tail>>::Output })
            {
                Some(tail) => {
                    let head = &projection_param_types[index];
                    quote::quote! { #head: crate::HAppend<#tail>, }
                }
                None => quote::quote! {},
            }
        })
        .collect::<Vec<_>>();
    quote::quote! {
        impl<#(#types),*> ProjectionShape for (#(#types,)*)
        where
            #(
                #types: ProjectionShape,
                <#types as ProjectionShape>::Row: Send,
                for<'any> <#types as ProjectionShape>::Exprs<'any>: Projectable,
            )*
            for<'scope> #exprs_tuple: Projectable,
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

            fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
                _ = alias;
                (
                    #(#types::exprs(alias),)*
                )
            }

            fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
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
            #(
                #types: Projectable,
            )*
        {
            type Rebound<'scope> = (
                #(<#types as Projectable>::Rebound<'scope>,)*
            );

            fn re_alias<'scope>(&self, alias: SourceAlias) -> Self::Rebound<'scope> {
                (
                    #(self.#fields.re_alias_with_prefix(alias, #prefixes),)*
                )
            }

            fn re_alias_with_prefix<'scope>(
                &self,
                alias: SourceAlias,
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

        impl<RenderBackend, #(#types),*> RenderProjectable<RenderBackend> for (#(#types,)*)
        where
            RenderBackend: ::squealy::Backend,
            #(
                #types: RenderProjectable<RenderBackend>,
            )*
        {
            fn visit_projection<V>(&self, visitor: &mut V) -> Result<(), V::Error>
            where
                V: ProjectionVisitor<Backend = RenderBackend>,
            {
                #(
                    self.#fields.visit_projection_with_prefix(#prefixes, visitor)?;
                )*
                Ok(())
            }

            fn visit_projection_with_prefix<V>(
                &self,
                prefix: &str,
                visitor: &mut V,
            ) -> Result<(), V::Error>
            where
                V: ProjectionVisitor<Backend = RenderBackend>,
            {
                #(
                    self.#fields
                        .visit_projection_with_prefix(&format!("{prefix}_{}", #prefixes), visitor)?;
                )*
                Ok(())
            }
        }

        impl<'scope, #(#types),*> ReturningProjection<'scope> for (#(#types,)*)
        where
            #(#types: ReturningProjection<'scope>,)*
            (#(#types,)*): Projectable,
            #returning_shape_tuple: ProjectionShape,
        {
            type Shape = (
                #(<#types as ReturningProjection<'scope>>::Shape,)*
            );
        }

        impl<#(#types),*> ::squealy::ProjectionClass for (#(#types,)*)
        where
            #first_type: ::squealy::ProjectionClass,
            #(
                #rest_types: ::squealy::ProjectionClass<
                    Class = <#first_type as ::squealy::ProjectionClass>::Class,
                >,
            )*
        {
            type Class = <#first_type as ::squealy::ProjectionClass>::Class;
        }

        // A tuple projection is column-free only when every element is (used to validate a
        // whole-table-aggregate `HAVING`; a bare column in any element leaves the tuple without an
        // impl, so `select` is rejected in that state).
        impl<#(#types),*> ::squealy::ProjectionColumns for (#(#types,)*)
        where
            #(#types: ::squealy::ProjectionColumns<Columns = ::squealy::ColumnFree>,)*
        {
            type Columns = ::squealy::ColumnFree;
        }

        impl<#(#types),*> crate::ProjectionParams for (#(#types,)*)
        where
            #( #types: crate::ProjectionParams, )*
            #(#projection_params_append_bounds)*
        {
            type Params = #projection_params_folded;
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
