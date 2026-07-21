//! Emits Rust construction tokens for a lowered schema-attribute expression.
//!
//! The derive macros parse a `check = "..."` / index expression string into a structural `ExprNode` at
//! expansion time with the private parser's neutral `Generic` dialect. This module turns
//! that `ExprNode` back into tokens that build the equivalent `::squealy::ExprNode` value in the
//! generated code, so the neutral model carries the structural form (not a raw string) and each backend
//! renders it in its own dialect.
//!
//! Only the scalar/constraint subset the reverse parser produces for a check / index expression is
//! handled; any other variant is a parser bug and panics with a clear message (surfaced as a macro
//! error rather than silently emitting wrong code).

use crate::expr_parser::{
	parse_check, parses, ArithmeticOp, CompareOp, ExprNode, LogicalOp, ParseExpressionError,
	ScalarFunc, SqlDialect,
};
use proc_macro2::TokenStream;
use quote::quote;

/// Renders a column's `check = "..."` attribute as the `Option<::squealy::ExprNode>` body of the
/// generated `Column::check`:
/// - `None` when absent;
/// - `Some(<structural expr>)` when the string lowers to a structural node in the neutral dialect;
/// - `Some(ExprNode::Raw("..."))` when it parses as valid SQL but is outside the structural subset the
///   reverse parser handles (`%` modulo, a backend-specific function) — OR when it is backend-specific
///   syntax the neutral `Generic` dialect cannot parse but a concrete backend can (e.g. PostgreSQL JSONB
///   `metadata ? 'key'`). It is preserved verbatim so the check renders exactly as authored;
/// - a `compile_error!` (with a `None` fallback so exactly one clear error surfaces) only when NO SQL
///   dialect can parse the string — a genuine authoring mistake worth catching at compile time.
pub(crate) fn check_option_tokens(expr: Option<&str>) -> TokenStream {
	let Some(expr) = expr else {
		return quote! { ::std::option::Option::None };
	};
	let raw = quote! {
			::std::option::Option::Some(::squealy::ExprNode::Raw(::std::string::String::from(#expr)))
	};
	match parse_check(expr) {
		Ok(node) => {
			let tokens = expr_tokens(&node);
			quote! { ::std::option::Option::Some(#tokens) }
		}
		// Valid neutral SQL outside the structural subset → preserved verbatim.
		Err(ParseExpressionError::Unsupported) => raw,
		// `Generic` could not parse it. It may still be valid *backend-specific* syntax (PostgreSQL
		// JSONB operators, etc.); if any concrete backend parses it, keep it verbatim rather than reject a
		// check that used to compile. Only a string no dialect can parse is a real authoring error.
		Err(ParseExpressionError::Invalid(error)) => {
			let parses_somewhere = [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite]
				.into_iter()
				.any(|dialect| parses(expr, dialect));
			if parses_somewhere {
				raw
			} else {
				let message = format!("invalid `check` expression `{expr}`: {error}");
				quote! { { ::std::compile_error!(#message); ::std::option::Option::None } }
			}
		}
	}
}

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
		ExprNode::Function { name, args } => {
			let args = args.iter().map(expr_tokens);
			quote! {
					::squealy::ExprNode::Function {
							name: ::std::string::String::from(#name),
							args: ::std::vec![ #(#args),* ],
					}
			}
		}
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
		ArithmeticOp::Modulo => quote!(Modulo),
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn routes_check_expressions_correctly() {
		// A check inside the structural grammar lowers to a structural node.
		let structural = check_option_tokens(Some("qty >= 0")).to_string();
		assert!(structural.contains("Compare"), "{structural}");
		assert!(!structural.contains("compile_error"), "{structural}");

		// `%` modulo lowers structurally — a neutral `Modulo` arithmetic node, not a `Raw` string.
		let modulo = check_option_tokens(Some("amount % 2 = 0")).to_string();
		assert!(modulo.contains("Modulo"), "{modulo}");
		assert!(!modulo.contains("Raw"), "{modulo}");
		assert!(!modulo.contains("compile_error"), "{modulo}");

		// Backend-specific syntax (PostgreSQL JSONB) is preserved as `Raw`, never a compile error.
		let jsonb = check_option_tokens(Some("metadata ? 'key'")).to_string();
		assert!(jsonb.contains("Raw"), "{jsonb}");
		assert!(!jsonb.contains("compile_error"), "{jsonb}");

		// Genuinely malformed SQL — parses in no dialect — is a compile error.
		let malformed = check_option_tokens(Some("qty >")).to_string();
		assert!(malformed.contains("compile_error"), "{malformed}");

		// No attribute → None.
		assert!(check_option_tokens(None).to_string().contains("None"));
	}
}
