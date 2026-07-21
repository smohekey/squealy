//! Private expression parser used only by schema attributes.

use sqlparser::ast::{
	BinaryOperator, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, UnaryOperator,
	Value,
};
use sqlparser::dialect::{
	Dialect as ParserDialect, GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

#[derive(Clone, Copy, Debug)]
pub(crate) enum SqlDialect {
	Postgres,
	Mysql,
	Sqlite,
	Generic,
}

impl SqlDialect {
	fn parser(self) -> Box<dyn ParserDialect> {
		match self {
			Self::Postgres => Box::new(PostgreSqlDialect {}),
			Self::Mysql => Box::new(MySqlDialect {}),
			Self::Sqlite => Box::new(SQLiteDialect {}),
			Self::Generic => Box::new(GenericDialect {}),
		}
	}
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ArithmeticOp {
	Add,
	Subtract,
	Multiply,
	Divide,
	Modulo,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CompareOp {
	Equals,
	NotEquals,
	LessThan,
	LessThanOrEquals,
	GreaterThan,
	GreaterThanOrEquals,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LogicalOp {
	And,
	Or,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ScalarFunc {
	Lower,
	Upper,
	Length,
	Trim,
	Concat,
	Substring,
}

#[derive(Debug)]
pub(crate) enum ExprNode {
	BareColumn {
		column: String,
	},
	Column {
		alias: String,
		column: String,
	},
	Literal(String),
	Binary {
		op: ArithmeticOp,
		left: Box<Self>,
		right: Box<Self>,
	},
	Compare {
		op: CompareOp,
		left: Box<Self>,
		right: Box<Self>,
	},
	Logical {
		op: LogicalOp,
		left: Box<Self>,
		right: Box<Self>,
	},
	Not(Box<Self>),
	IsNull {
		negated: bool,
		operand: Box<Self>,
	},
	Like {
		case_insensitive: bool,
		negated: bool,
		operand: Box<Self>,
		pattern: Box<Self>,
	},
	In {
		negated: bool,
		operand: Box<Self>,
		items: Vec<Self>,
	},
	Between {
		negated: bool,
		operand: Box<Self>,
		low: Box<Self>,
		high: Box<Self>,
	},
	ScalarFn {
		func: ScalarFunc,
		args: Vec<Self>,
	},
	Function {
		name: String,
		args: Vec<Self>,
	},
}

pub(crate) enum ParseExpressionError {
	Invalid(String),
	Unsupported,
}

pub(crate) fn parse_check(sql: &str) -> Result<ExprNode, ParseExpressionError> {
	let parsed = parse_expr(sql, SqlDialect::Generic).map_err(ParseExpressionError::Invalid)?;
	lower(&parsed, SqlDialect::Generic).map_err(|()| ParseExpressionError::Unsupported)
}

pub(crate) fn parses(sql: &str, dialect: SqlDialect) -> bool {
	parse_expr(sql, dialect).is_ok()
}

fn parse_expr(sql: &str, dialect: SqlDialect) -> Result<Expr, String> {
	let binding = dialect.parser();
	let mut parser = Parser::new(binding.as_ref())
		.try_with_sql(sql)
		.map_err(|error| error.to_string())?;
	let expr = parser.parse_expr().map_err(|error| error.to_string())?;
	let trailing = &parser.peek_token().token;
	if *trailing != Token::EOF {
		return Err(format!("unexpected trailing token `{trailing}`"));
	}
	Ok(expr)
}

fn lower(expr: &Expr, dialect: SqlDialect) -> Result<ExprNode, ()> {
	match expr {
		Expr::Nested(inner) => lower(inner, dialect),
		Expr::Identifier(ident) => Ok(ExprNode::BareColumn {
			column: ident.value.clone(),
		}),
		Expr::CompoundIdentifier(parts) => match parts.as_slice() {
			[alias, column] => Ok(ExprNode::Column {
				alias: alias.value.clone(),
				column: column.value.clone(),
			}),
			_ => Err(()),
		},
		Expr::Value(value) => lower_value(&value.value),
		Expr::UnaryOp { op, expr } => lower_unary(*op, expr, dialect),
		Expr::BinaryOp { left, op, right } => lower_binary(left, op, right, dialect),
		Expr::IsNull(operand) => Ok(ExprNode::IsNull {
			negated: false,
			operand: Box::new(lower(operand, dialect)?),
		}),
		Expr::IsNotNull(operand) => Ok(ExprNode::IsNull {
			negated: true,
			operand: Box::new(lower(operand, dialect)?),
		}),
		Expr::Between {
			expr,
			negated,
			low,
			high,
		} => Ok(ExprNode::Between {
			negated: *negated,
			operand: Box::new(lower(expr, dialect)?),
			low: Box::new(lower(low, dialect)?),
			high: Box::new(lower(high, dialect)?),
		}),
		Expr::InList {
			expr,
			list,
			negated,
		} => Ok(ExprNode::In {
			negated: *negated,
			operand: Box::new(lower(expr, dialect)?),
			items: list
				.iter()
				.map(|item| lower(item, dialect))
				.collect::<Result<_, _>>()?,
		}),
		Expr::Like {
			negated,
			expr,
			pattern,
			escape_char: None,
			any: false,
		} => Ok(ExprNode::Like {
			case_insensitive: false,
			negated: *negated,
			operand: Box::new(lower(expr, dialect)?),
			pattern: Box::new(lower(pattern, dialect)?),
		}),
		Expr::ILike {
			negated,
			expr,
			pattern,
			escape_char: None,
			any: false,
		} => Ok(ExprNode::Like {
			case_insensitive: true,
			negated: *negated,
			operand: Box::new(lower(expr, dialect)?),
			pattern: Box::new(lower(pattern, dialect)?),
		}),
		Expr::Substring {
			expr,
			substring_from: Some(from),
			substring_for: Some(length),
			..
		} => Ok(ExprNode::ScalarFn {
			func: ScalarFunc::Substring,
			args: vec![
				lower(expr, dialect)?,
				lower(from, dialect)?,
				lower(length, dialect)?,
			],
		}),
		Expr::Trim {
			expr,
			trim_where: None,
			trim_what: None,
			trim_characters: None,
		} => Ok(ExprNode::ScalarFn {
			func: ScalarFunc::Trim,
			args: vec![lower(expr, dialect)?],
		}),
		Expr::Function(function) => lower_function(function, dialect),
		_ => Err(()),
	}
}

fn lower_value(value: &Value) -> Result<ExprNode, ()> {
	let value = match value {
		Value::Number(number, _) => number.clone(),
		Value::SingleQuotedString(value) => format!("'{}'", value.replace('\'', "''")),
		Value::Boolean(true) => "TRUE".into(),
		Value::Boolean(false) => "FALSE".into(),
		Value::Null => "NULL".into(),
		_ => return Err(()),
	};
	Ok(ExprNode::Literal(value))
}

fn lower_unary(op: UnaryOperator, operand: &Expr, dialect: SqlDialect) -> Result<ExprNode, ()> {
	match op {
		UnaryOperator::Minus | UnaryOperator::Plus => {
			if let Expr::Value(value) = operand
				&& let Value::Number(number, _) = &value.value
			{
				let sign = if matches!(op, UnaryOperator::Minus) {
					"-"
				} else {
					""
				};
				Ok(ExprNode::Literal(format!("{sign}{number}")))
			} else {
				Err(())
			}
		}
		UnaryOperator::Not => Ok(ExprNode::Not(Box::new(lower(operand, dialect)?))),
		_ => Err(()),
	}
}

fn lower_binary(
	left: &Expr,
	op: &BinaryOperator,
	right: &Expr,
	dialect: SqlDialect,
) -> Result<ExprNode, ()> {
	let arithmetic = match op {
		BinaryOperator::Plus => Some(ArithmeticOp::Add),
		BinaryOperator::Minus => Some(ArithmeticOp::Subtract),
		BinaryOperator::Multiply => Some(ArithmeticOp::Multiply),
		BinaryOperator::Divide => Some(ArithmeticOp::Divide),
		BinaryOperator::Modulo => Some(ArithmeticOp::Modulo),
		_ => None,
	};
	if let Some(op) = arithmetic {
		return Ok(ExprNode::Binary {
			op,
			left: Box::new(lower(left, dialect)?),
			right: Box::new(lower(right, dialect)?),
		});
	}

	let compare = match op {
		BinaryOperator::Eq => Some(CompareOp::Equals),
		BinaryOperator::NotEq => Some(CompareOp::NotEquals),
		BinaryOperator::Lt => Some(CompareOp::LessThan),
		BinaryOperator::LtEq => Some(CompareOp::LessThanOrEquals),
		BinaryOperator::Gt => Some(CompareOp::GreaterThan),
		BinaryOperator::GtEq => Some(CompareOp::GreaterThanOrEquals),
		_ => None,
	};
	if let Some(op) = compare {
		return Ok(ExprNode::Compare {
			op,
			left: Box::new(lower(left, dialect)?),
			right: Box::new(lower(right, dialect)?),
		});
	}

	let logical = match op {
		BinaryOperator::And => Some(LogicalOp::And),
		BinaryOperator::Or => Some(LogicalOp::Or),
		_ => None,
	};
	if let Some(op) = logical {
		return Ok(ExprNode::Logical {
			op,
			left: Box::new(lower(left, dialect)?),
			right: Box::new(lower(right, dialect)?),
		});
	}

	Err(())
}

fn lower_function(function: &Function, dialect: SqlDialect) -> Result<ExprNode, ()> {
	if function.over.is_some()
		|| function.filter.is_some()
		|| function.null_treatment.is_some()
		|| !function.within_group.is_empty()
		|| function.parameters != FunctionArguments::None
	{
		return Err(());
	}
	let ident = match function.name.0.as_slice() {
		[part] => part.as_ident().ok_or(())?,
		_ => return Err(()),
	};
	let quoted = ident.quote_style.is_some();
	let name = ident.value.to_ascii_lowercase();
	let args = match &function.args {
		FunctionArguments::List(list)
			if list.duplicate_treatment.is_none() && list.clauses.is_empty() =>
		{
			list
				.args
				.iter()
				.map(|arg| match arg {
					FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => lower(expr, dialect),
					_ => Err(()),
				})
				.collect::<Result<Vec<_>, _>>()?
		}
		_ => return Err(()),
	};

	match name.as_str() {
		"lower" if args.len() == 1 => Ok(ExprNode::ScalarFn {
			func: ScalarFunc::Lower,
			args,
		}),
		"upper" if args.len() == 1 => Ok(ExprNode::ScalarFn {
			func: ScalarFunc::Upper,
			args,
		}),
		"length" | "char_length" if args.len() == 1 => Ok(ExprNode::ScalarFn {
			func: ScalarFunc::Length,
			args,
		}),
		"trim" if args.len() == 1 => Ok(ExprNode::ScalarFn {
			func: ScalarFunc::Trim,
			args,
		}),
		"concat" => Ok(ExprNode::ScalarFn {
			func: ScalarFunc::Concat,
			args,
		}),
		_ if !quoted && !args.iter().any(|arg| matches!(arg, ExprNode::Literal(_))) => {
			Ok(ExprNode::Function { name, args })
		}
		_ => Err(()),
	}
}
