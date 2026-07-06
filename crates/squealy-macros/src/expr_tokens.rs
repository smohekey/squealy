//! Emits Rust construction tokens for a lowered [`squealy_ir::ExprNode`].
//!
//! The derive macros parse a `check = "..."` / index expression string into a structural `ExprNode` at
//! expansion time (via `squealy-parse`, in the neutral `Generic` authoring dialect). This module turns
//! that `ExprNode` back into tokens that build the equivalent `::squealy::ExprNode` value in the
//! generated code, so the neutral model carries the structural form (not a raw string) and each backend
//! renders it in its own dialect.
//!
//! Only the scalar/constraint subset the reverse parser produces for a check / index expression is
//! handled; any other variant is a parser bug and panics with a clear message (surfaced as a macro
//! error rather than silently emitting wrong code).

use proc_macro2::TokenStream;
use quote::quote;
use squealy_ir::{ArithmeticOp, CompareOp, ExprNode, LogicalOp, ScalarFunc};

/// Renders `node` as a `TokenStream` constructing the equivalent `::squealy::ExprNode` at runtime.
pub(crate) fn expr_tokens(node: &ExprNode) -> TokenStream {
    match node {
        ExprNode::BareColumn { column } => quote! {
            ::squealy::ExprNode::BareColumn { column: ::std::string::String::from(#column) }
        },
        ExprNode::Column { alias, column } => quote! {
            ::squealy::ExprNode::Column {
                alias: ::std::string::String::from(#alias),
                column: ::std::string::String::from(#column),
            }
        },
        ExprNode::Literal(text) => quote! {
            ::squealy::ExprNode::Literal(::std::string::String::from(#text))
        },
        ExprNode::Binary { op, left, right } => {
            let op = arithmetic_tokens(*op);
            let (left, right) = (boxed(left), boxed(right));
            quote! { ::squealy::ExprNode::Binary { op: #op, left: #left, right: #right } }
        }
        ExprNode::Compare { op, left, right } => {
            let op = compare_tokens(*op);
            let (left, right) = (boxed(left), boxed(right));
            quote! { ::squealy::ExprNode::Compare { op: #op, left: #left, right: #right } }
        }
        ExprNode::Logical { op, left, right } => {
            let op = logical_tokens(*op);
            let (left, right) = (boxed(left), boxed(right));
            quote! { ::squealy::ExprNode::Logical { op: #op, left: #left, right: #right } }
        }
        ExprNode::Not(operand) => {
            let operand = boxed(operand);
            quote! { ::squealy::ExprNode::Not(#operand) }
        }
        ExprNode::IsNull { negated, operand } => {
            let operand = boxed(operand);
            quote! { ::squealy::ExprNode::IsNull { negated: #negated, operand: #operand } }
        }
        ExprNode::Like {
            case_insensitive,
            negated,
            operand,
            pattern,
        } => {
            let (operand, pattern) = (boxed(operand), boxed(pattern));
            quote! {
                ::squealy::ExprNode::Like {
                    case_insensitive: #case_insensitive,
                    negated: #negated,
                    operand: #operand,
                    pattern: #pattern,
                }
            }
        }
        ExprNode::In {
            negated,
            operand,
            items,
        } => {
            let operand = boxed(operand);
            let items = items.iter().map(expr_tokens);
            quote! {
                ::squealy::ExprNode::In {
                    negated: #negated,
                    operand: #operand,
                    items: ::std::vec![ #(#items),* ],
                }
            }
        }
        ExprNode::Between {
            negated,
            operand,
            low,
            high,
        } => {
            let (operand, low, high) = (boxed(operand), boxed(low), boxed(high));
            quote! {
                ::squealy::ExprNode::Between {
                    negated: #negated,
                    operand: #operand,
                    low: #low,
                    high: #high,
                }
            }
        }
        ExprNode::ScalarFn { func, args } => {
            let func = scalar_fn_tokens(*func);
            let args = args.iter().map(expr_tokens);
            quote! { ::squealy::ExprNode::ScalarFn { func: #func, args: ::std::vec![ #(#args),* ] } }
        }
        other => panic!(
            "internal error: the reverse parser produced an expression node the constraint token \
             emitter does not handle: {other:?}"
        ),
    }
}

fn boxed(node: &ExprNode) -> TokenStream {
    let inner = expr_tokens(node);
    quote! { ::std::boxed::Box::new(#inner) }
}

fn arithmetic_tokens(op: ArithmeticOp) -> TokenStream {
    let variant = match op {
        ArithmeticOp::Add => quote!(Add),
        ArithmeticOp::Subtract => quote!(Subtract),
        ArithmeticOp::Multiply => quote!(Multiply),
        ArithmeticOp::Divide => quote!(Divide),
    };
    quote! { ::squealy::ArithmeticOp::#variant }
}

fn compare_tokens(op: CompareOp) -> TokenStream {
    let variant = match op {
        CompareOp::Equals => quote!(Equals),
        CompareOp::NotEquals => quote!(NotEquals),
        CompareOp::LessThan => quote!(LessThan),
        CompareOp::LessThanOrEquals => quote!(LessThanOrEquals),
        CompareOp::GreaterThan => quote!(GreaterThan),
        CompareOp::GreaterThanOrEquals => quote!(GreaterThanOrEquals),
    };
    quote! { ::squealy::CompareOp::#variant }
}

fn logical_tokens(op: LogicalOp) -> TokenStream {
    let variant = match op {
        LogicalOp::And => quote!(And),
        LogicalOp::Or => quote!(Or),
    };
    quote! { ::squealy::LogicalOp::#variant }
}

fn scalar_fn_tokens(func: ScalarFunc) -> TokenStream {
    let variant = match func {
        ScalarFunc::Lower => quote!(Lower),
        ScalarFunc::Upper => quote!(Upper),
        ScalarFunc::Length => quote!(Length),
        ScalarFunc::Trim => quote!(Trim),
        ScalarFunc::Concat => quote!(Concat),
        ScalarFunc::Substring => quote!(Substring),
    };
    quote! { ::squealy::ScalarFunc::#variant }
}
