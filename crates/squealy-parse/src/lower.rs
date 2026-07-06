//! AST → neutral-model lowering — the structural inverse of the renderers.
//!
//! The renderers walk [`squealy_ir::ExprNode`] / [`squealy_ir::ViewQueryModel`] into dialect SQL
//! (`view_render` and each backend's DDL writer). Lowering walks the [`sqlparser`] AST the
//! other way. It is dialect-parameterized by [`SqlDialect`] because the same syntax can mean different
//! things across dialects (`||` is concatenation in PostgreSQL/SQLite but logical `OR` in MySQL), and
//! because inverting the renderer's per-dialect idioms requires knowing which dialect emitted them.
//!
//! Lowering leans on [`crate::normalize`] (the fold/unwind catalogue) to peel the renderer's own idioms
//! — full parenthesization, the float-cast division form, `||`/`substr` spellings — while building the
//! neutral node.
//!
//! # Status
//!
//! [`lower_expr`] covers the **scalar** grammar the renderer emits for `CHECK` / generated-column /
//! index expressions: columns (qualified + unqualified), literals, arithmetic, comparison, logical
//! `AND`/`OR`/`NOT`, `IS [NOT] NULL`, `IN (<list>)`, `BETWEEN`, `LIKE`/`ILIKE`, and the closed
//! scalar-function set (`LOWER`/`UPPER`/`CHAR_LENGTH`/`TRIM`/`CONCAT`/`SUBSTRING`). Anything outside it
//! (`%` modulo — no neutral node; a general `CAST` — dialect-ambiguous target names; general/user
//! functions; subqueries; `CASE`) yields [`ReadError::NotYetLowered`]. View-body lowering
//! ([`lower_query`]) is a later phase.

use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Query, UnaryOperator, Value,
};
use squealy_ir::{ArithmeticOp, CompareOp, ExprNode, LogicalOp, ScalarFunc, ViewQueryModel};

use crate::{ReadError, SqlDialect};

/// Lowers a parsed scalar expression into an [`ExprNode`].
///
/// Handles the grammar the renderer emits for a `CHECK` / generated-column / index expression (see the
/// [module docs](self)); shapes outside it return [`ReadError::NotYetLowered`] naming the offending
/// node, so a caller (the round-trip harness, a macro, live introspection) sees exactly what remains.
pub fn lower_expr(expr: &Expr, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    lower(expr, dialect)
}

/// Lowers a parsed `SELECT` query (a view body) into a [`ViewQueryModel`].
///
/// Phase 0 stub: returns [`ReadError::NotYetLowered`]. View-body reconstruction (projection/from/
/// joins/filter + view-output type inference) is a later phase.
pub fn lower_query(query: &Query, dialect: SqlDialect) -> Result<ViewQueryModel, ReadError> {
    let _ = dialect;
    Err(ReadError::NotYetLowered(format!("query body `{query}`")))
}

fn not_yet(what: impl std::fmt::Display) -> ReadError {
    ReadError::NotYetLowered(what.to_string())
}

fn b(node: ExprNode) -> Box<ExprNode> {
    Box::new(node)
}

fn lower(expr: &Expr, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    match expr {
        // Full parenthesization: the renderer wraps every operator/predicate node in `(...)`. Strip it
        // transparently — precedence is already fixed by the tree shape.
        Expr::Nested(inner) => lower(inner, dialect),

        // A bare column (unqualified, as constraint/index/generated expressions name their own table's
        // columns) or a qualified `alias.column` (a view body binds columns to a source alias). The
        // renderer always quotes these; `sqlparser` hands back the unquoted value.
        Expr::Identifier(ident) => Ok(ExprNode::BareColumn {
            column: fold_ident(ident),
        }),
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            [alias, column] => Ok(ExprNode::Column {
                alias: fold_ident(alias),
                column: fold_ident(column),
            }),
            _ => Err(not_yet(format!(
                "compound identifier with {} parts",
                parts.len()
            ))),
        },

        Expr::Value(value) => lower_value(&value.value),

        Expr::UnaryOp { op, expr } => lower_unary(*op, expr, dialect),
        Expr::BinaryOp { left, op, right } => lower_binary(left, op, right, dialect),

        Expr::IsNull(operand) => Ok(ExprNode::IsNull {
            negated: false,
            operand: b(lower(operand, dialect)?),
        }),
        Expr::IsNotNull(operand) => Ok(ExprNode::IsNull {
            negated: true,
            operand: b(lower(operand, dialect)?),
        }),

        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Ok(ExprNode::Between {
            negated: *negated,
            operand: b(lower(expr, dialect)?),
            low: b(lower(low, dialect)?),
            high: b(lower(high, dialect)?),
        }),

        Expr::InList {
            expr,
            list,
            negated,
        } => Ok(ExprNode::In {
            negated: *negated,
            operand: b(lower(expr, dialect)?),
            items: list
                .iter()
                .map(|item| lower(item, dialect))
                .collect::<Result<_, _>>()?,
        }),

        // `LIKE`/`NOT LIKE` → the case-*sensitive* `Like`. Only PostgreSQL spells the case-insensitive
        // form distinctly (`ILIKE`, the arm below); MySQL/SQLite `LIKE` is already case-insensitive and
        // the renderer emits plain `LIKE` for either flag state (the default `write_like_operator` ignores
        // it), so a bare `LIKE` is the exact inverse of the renderer's non-`ILIKE` output. A
        // `case_insensitive: true` model is therefore only structurally recoverable on PostgreSQL — but
        // squealy never emits one for a MySQL/SQLite constraint (`ILIKE` is PostgreSQL-only syntax; those
        // dialects' checks use plain `LIKE`). (The `ESCAPE` clause has no neutral node → not lowered.)
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char: None,
            any: false,
        } => Ok(ExprNode::Like {
            case_insensitive: false,
            negated: *negated,
            operand: b(lower(expr, dialect)?),
            pattern: b(lower(pattern, dialect)?),
        }),
        // `ILIKE`/`NOT ILIKE` (PostgreSQL) — the renderer's `case_insensitive` `Like` node.
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char: None,
            any: false,
        } => Ok(ExprNode::Like {
            case_insensitive: true,
            negated: *negated,
            operand: b(lower(expr, dialect)?),
            pattern: b(lower(pattern, dialect)?),
        }),

        // A general user `CAST(x AS ty)` is deliberately NOT lowered here: inverting the cast target
        // name is dialect-specific and, for MySQL, ambiguous — its cast vocabulary (`SIGNED`, `UNSIGNED`,
        // `DOUBLE`, `CHAR`, `DECIMAL`) does not map one-to-one back to a neutral `SqlType` width. squealy
        // emits no general cast in a scalar constraint today (the only cast in this position is the
        // float-division idiom, peeled at the `Divide` operator via `float_cast_operand`), so a general
        // cast falls through to `NotYetLowered`; proper cross-dialect cast inversion lands with the
        // model-field migration that first produces such casts.
        //
        // EXCEPT PostgreSQL's `pg_get_constraintdef` synthesizes a *redundant* `::type` cast on a literal
        // whose natural type already matches: a number to a numeric type (`0` → `(0)::numeric`) or a
        // string to a text type (`'x'` → `('x')::text`). Strip only those back to the bare literal (so a
        // published check re-plans to empty) — NOT a cast that *converts* (`'Infinity'::float8`,
        // `'2020-01-01'::date`), which is a meaningful user cast and is left `NotYetLowered` (→ `Raw`).
        Expr::Cast {
            kind: CastKind::DoubleColon,
            expr,
            data_type,
            ..
        } if dialect == SqlDialect::Postgres
            && is_redundant_literal_cast(strip_nested(expr), data_type) =>
        {
            lower(strip_nested(expr), dialect)
        }

        // PostgreSQL deparses `x IN (a, b, c)` as `x = ANY (ARRAY[a, b, c])` and `x NOT IN (…)` as
        // `x <> ALL (ARRAY[…])`. Recover the neutral `In`. (These operators are PostgreSQL-only syntax, so
        // they never arrive on another dialect.)
        Expr::AnyOp {
            left,
            compare_op: BinaryOperator::Eq,
            right,
            is_some: false,
        } => lower_array_membership(left, right, false, dialect),
        Expr::AllOp {
            left,
            compare_op: BinaryOperator::NotEq,
            right,
        } => lower_array_membership(left, right, true, dialect),

        // `SUBSTRING(s FROM start FOR len)` and SQLite's `substr(s, start, len)` both parse to this node.
        // The renderer only emits the three-argument *positional* form (integer bounds), so both bounds
        // must be present and neither may be a string — PostgreSQL overloads the same `FROM … FOR …` shape
        // as a POSIX-regex extractor when the bounds are string patterns (`SUBSTRING(s FROM 'p' FOR 'e')`),
        // which is a different operation and is left `NotYetLowered` rather than mis-lowered to positional.
        Expr::Substring {
            expr,
            substring_from: Some(from),
            substring_for: Some(len),
            ..
        } if !is_string_literal(from) && !is_string_literal(len) => Ok(ExprNode::ScalarFn {
            func: ScalarFunc::Substring,
            args: vec![
                lower(expr, dialect)?,
                lower(from, dialect)?,
                lower(len, dialect)?,
            ],
        }),

        // Plain `TRIM(s)` — the renderer emits no `LEADING`/`TRAILING`/`FROM` variants.
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

        other => Err(not_yet(format!("scalar expression `{other}`"))),
    }
}

/// Formats a parsed literal into the already-rendered text an [`ExprNode::Literal`] carries, so it
/// re-renders byte-identically. The renderer emits literals verbatim, so this is the canonical form.
fn lower_value(value: &Value) -> Result<ExprNode, ReadError> {
    let text = match value {
        Value::Number(number, _) => number.clone(),
        Value::SingleQuotedString(string) => format!("'{}'", string.replace('\'', "''")),
        Value::Boolean(true) => "TRUE".to_owned(),
        Value::Boolean(false) => "FALSE".to_owned(),
        Value::Null => "NULL".to_owned(),
        other => return Err(not_yet(format!("literal `{other}`"))),
    };
    Ok(ExprNode::Literal(text))
}

fn lower_unary(
    op: UnaryOperator,
    operand: &Expr,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    match op {
        // A signed numeric literal (`-5`) parses as unary minus over the magnitude; fold it back into
        // the literal text so it re-renders as the same `-5` (there is no neutral negation node).
        UnaryOperator::Minus | UnaryOperator::Plus => {
            if let Expr::Value(value) = operand
                && let Value::Number(number, _) = &value.value
            {
                let sign = if matches!(op, UnaryOperator::Minus) {
                    "-"
                } else {
                    ""
                };
                return Ok(ExprNode::Literal(format!("{sign}{number}")));
            }
            Err(not_yet(format!("unary `{op}` on a non-literal operand")))
        }
        UnaryOperator::Not => Ok(ExprNode::Not(b(lower(operand, dialect)?))),
        other => Err(not_yet(format!("unary operator `{other}`"))),
    }
}

fn lower_binary(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    // Arithmetic.
    let arithmetic = match op {
        BinaryOperator::Plus => Some(ArithmeticOp::Add),
        BinaryOperator::Minus => Some(ArithmeticOp::Subtract),
        BinaryOperator::Multiply => Some(ArithmeticOp::Multiply),
        BinaryOperator::Divide => Some(ArithmeticOp::Divide),
        _ => None,
    };
    if let Some(op) = arithmetic {
        if op == ArithmeticOp::Divide {
            // PostgreSQL/SQLite render a neutral `Divide` as the paired float-cast form
            // `CAST(a AS <float>) / CAST(b AS <float>)` (`integer_division_needs_float_cast`); peel it.
            if let (Some(left), Some(right)) = (
                float_cast_operand(left, dialect),
                float_cast_operand(right, dialect),
            ) {
                return Ok(ExprNode::Binary {
                    op,
                    left: b(lower(left, dialect)?),
                    right: b(lower(right, dialect)?),
                });
            }
            // A *bare* `/` is fractional only where the renderer emits it bare — MySQL — or when reading
            // neutral authored SQL (`Generic`), where `/` denotes the neutral (always-fractional) divide.
            // On PostgreSQL/SQLite a bare `/` is *integer* division (a different operation, no neutral
            // node); the neutral `Divide` re-renders as the float-cast form, so lowering it there would
            // change semantics.
            if !matches!(dialect, SqlDialect::Mysql | SqlDialect::Generic) {
                return Err(not_yet(
                    "bare `/` division (integer division has no neutral node outside MySQL)",
                ));
            }
        }
        return Ok(ExprNode::Binary {
            op,
            left: b(lower(left, dialect)?),
            right: b(lower(right, dialect)?),
        });
    }

    // Comparison.
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
            left: b(lower(left, dialect)?),
            right: b(lower(right, dialect)?),
        });
    }

    match op {
        // SQL has no `IN ()`, so the renderer fixes an empty membership test with a constant-truth
        // sentinel: `<operand> IS NOT NULL AND 1 = 0` (an empty `IN`) / `<operand> IS NOT NULL OR 1 = 1`
        // (an empty `NOT IN`). Recover the sentinel to the original empty `In` node so it re-renders as
        // the sentinel rather than churning into a bare `Logical`.
        BinaryOperator::And => {
            if let Some(node) = recover_empty_in(left, right, false, dialect)? {
                return Ok(node);
            }
            Ok(ExprNode::Logical {
                op: LogicalOp::And,
                left: b(lower(left, dialect)?),
                right: b(lower(right, dialect)?),
            })
        }
        BinaryOperator::Or => {
            if let Some(node) = recover_empty_in(left, right, true, dialect)? {
                return Ok(node);
            }
            Ok(ExprNode::Logical {
                op: LogicalOp::Or,
                left: b(lower(left, dialect)?),
                right: b(lower(right, dialect)?),
            })
        }
        // `||` denotes concatenation on PostgreSQL/SQLite (where the renderer emits it for `Concat`), but
        // MySQL treats `||` as logical `OR` and the renderer uses `CONCAT(...)` there (see
        // `lower_function`). Only fold to `Concat` on the dialects where `||` is concatenation.
        BinaryOperator::StringConcat if pipe_is_concatenation(dialect) => Ok(ExprNode::ScalarFn {
            func: ScalarFunc::Concat,
            args: vec![lower(left, dialect)?, lower(right, dialect)?],
        }),
        // PostgreSQL's `pg_get_constraintdef` deparses `LIKE`/`ILIKE` as the operator forms
        // `~~`/`~~*` (and `NOT LIKE`/`NOT ILIKE` as `!~~`/`!~~*`). Recover the neutral `Like`.
        BinaryOperator::PGLikeMatch => Ok(like_node(false, false, left, right, dialect)?),
        BinaryOperator::PGNotLikeMatch => Ok(like_node(false, true, left, right, dialect)?),
        BinaryOperator::PGILikeMatch => Ok(like_node(true, false, left, right, dialect)?),
        BinaryOperator::PGNotILikeMatch => Ok(like_node(true, true, left, right, dialect)?),
        // `%` (no neutral arithmetic node), MySQL `||` (logical OR), and any other operator.
        other => Err(not_yet(format!("binary operator `{other}`"))),
    }
}

/// Builds a [`ExprNode::Like`] from an operand/pattern pair (used to invert PostgreSQL's `~~` operator
/// deparse of `LIKE`/`ILIKE`).
fn like_node(
    case_insensitive: bool,
    negated: bool,
    operand: &Expr,
    pattern: &Expr,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    Ok(ExprNode::Like {
        case_insensitive,
        negated,
        operand: b(lower(operand, dialect)?),
        pattern: b(lower(pattern, dialect)?),
    })
}

/// Recovers a neutral [`ExprNode::In`] from PostgreSQL's `<operand> = ANY (ARRAY[..])` /
/// `<operand> <> ALL (ARRAY[..])` deparse of `IN`/`NOT IN`. The right side must be an array literal.
fn lower_array_membership(
    operand: &Expr,
    array: &Expr,
    negated: bool,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    let Expr::Array(array) = array else {
        return Err(not_yet(format!(
            "`{}` membership over a non-array operand",
            if negated { "ALL" } else { "ANY" }
        )));
    };
    Ok(ExprNode::In {
        negated,
        operand: b(lower(operand, dialect)?),
        items: array
            .elem
            .iter()
            .map(|item| lower(item, dialect))
            .collect::<Result<_, _>>()?,
    })
}

/// Whether `expr` is a literal whose `::type` cast is a *redundant* one PostgreSQL's deparse synthesizes
/// on a literal already of that type — a number cast to a numeric type, or a string cast to a text type.
/// A cast that *converts* (a string to a float/date/etc.) is meaningful and is NOT redundant, so it is
/// not stripped.
fn is_redundant_literal_cast(expr: &Expr, data_type: &DataType) -> bool {
    let Expr::Value(value) = expr else {
        return false;
    };
    match &value.value {
        Value::Number(text, _) => {
            let integer_literal = !text.contains(['.', 'e', 'E']);
            if integer_literal {
                // An integer literal is exactly representable in every numeric type, so the cast is a
                // no-op regardless of target.
                is_numeric_type(data_type)
            } else {
                // A fractional literal survives only in a fractional type; casting it to an integer type
                // TRUNCATES (`(1.5)::integer` → `1`), which is a real conversion, not redundant.
                is_numeric_type(data_type) && !is_integer_type(data_type)
            }
        }
        // A string cast to an UNBOUNDED text type is a no-op. A length-bounded `varchar(n)`/`char(n)`
        // can truncate or pad, so it is NOT redundant (it falls through to `Raw`, where the string
        // canonicalizer keeps it comparable without dropping the cast).
        Value::SingleQuotedString(_) => matches!(
            data_type,
            DataType::Text
                | DataType::Varchar(None)
                | DataType::CharVarying(None)
                | DataType::CharacterVarying(None)
        ),
        _ => false,
    }
}

/// Whether `data_type` is a whole-number integer type (casting a fractional literal to it truncates).
/// A display-width arg (`int(11)`) does not change the value, so it is accepted.
fn is_integer_type(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::TinyInt(_)
            | DataType::SmallInt(_)
            | DataType::Int(_)
            | DataType::Int2(_)
            | DataType::Int4(_)
            | DataType::Int8(_)
            | DataType::Integer(_)
            | DataType::BigInt(_)
    )
}

/// Whether casting a literal to `data_type` is a guaranteed value-preserving no-op — an integer type,
/// or an **unbounded arbitrary-precision** `numeric`/`decimal`. Notably NOT a floating type
/// (`real`/`float4`/`float8`/`double precision`): a binary float cannot hold every integer or decimal
/// exactly (`(16777217)::real` rounds to `16777216`), so a float cast can change the value and is left as
/// `Raw` (kept comparable by the string canonicalizer) rather than stripped. A precision-bounded
/// `numeric(p, s)` can round too, so only the unbounded form is a no-op.
fn is_numeric_type(data_type: &DataType) -> bool {
    use sqlparser::ast::ExactNumberInfo::None as NoPrecision;
    is_integer_type(data_type)
        || matches!(
            data_type,
            DataType::Numeric(NoPrecision)
                | DataType::Decimal(NoPrecision)
                | DataType::Dec(NoPrecision)
        )
}

/// Resolves an identifier to the column name the model stores. An *unquoted* identifier is
/// case-insensitive in SQL and folds to lower case (PostgreSQL folds `Id` → `id`; the renderer then
/// re-quotes it, so it must already be the folded form to name the right column); a *quoted* identifier
/// is case-exact and preserved verbatim.
fn fold_ident(ident: &sqlparser::ast::Ident) -> String {
    if ident.quote_style.is_none() {
        ident.value.to_lowercase()
    } else {
        ident.value.clone()
    }
}

/// Strips [`Expr::Nested`] wrappers, returning the inner expression (PostgreSQL wraps a cast's literal
/// operand in parentheses: `(0)::numeric`).
fn strip_nested(expr: &Expr) -> &Expr {
    let mut current = expr;
    while let Expr::Nested(inner) = current {
        current = inner;
    }
    current
}

/// Recognizes the renderer's empty-`IN` sentinel and recovers the original empty [`ExprNode::In`].
///
/// An empty membership test renders as `<operand> IS NOT NULL AND 1 = 0` (`negated = false`) or
/// `<operand> IS NOT NULL OR 1 = 1` (`negated = true`), since SQL has no `IN ()`. Given the two sides of
/// the enclosing `AND`/`OR`, returns the empty `In` when they match that shape, else `None` (so a
/// genuine `Logical` falls through). Even a user who literally wrote the sentinel re-renders identically,
/// so the recovery is safe.
fn recover_empty_in(
    left: &Expr,
    right: &Expr,
    negated: bool,
    dialect: SqlDialect,
) -> Result<Option<ExprNode>, ReadError> {
    let Expr::IsNotNull(operand) = left else {
        return Ok(None);
    };
    // The right side is the constant `1 = 0` (`AND`, empty `IN`) or `1 = 1` (`OR`, empty `NOT IN`).
    let expected = if negated { "1" } else { "0" };
    let is_sentinel_constant = matches!(
        right,
        Expr::BinaryOp { left, op: BinaryOperator::Eq, right }
            if is_number(left, "1") && is_number(right, expected)
    );
    if !is_sentinel_constant {
        return Ok(None);
    }
    Ok(Some(ExprNode::In {
        negated,
        operand: b(lower(operand, dialect)?),
        items: Vec::new(),
    }))
}

/// Whether `expr` is the unsigned integer literal `value` (the sentinel's `1`/`0` constants).
fn is_number(expr: &Expr, value: &str) -> bool {
    matches!(
        expr,
        Expr::Value(v) if matches!(&v.value, Value::Number(number, _) if number == value)
    )
}

/// Whether `expr` is a string literal — used to reject PostgreSQL's regex `SUBSTRING(s FROM 'p' FOR 'e')`
/// overload, whose bounds are strings (the positional form squealy emits has integer bounds).
fn is_string_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(v) if matches!(
            &v.value,
            Value::SingleQuotedString(_)
                | Value::DoubleQuotedString(_)
                | Value::EscapedStringLiteral(_)
                | Value::NationalStringLiteral(_)
        )
    )
}

/// If `expr` is the `CAST(inner AS <float>)` wrapper the renderer applies to each operand of a neutral
/// `Divide` on `dialect`, returns `inner`; else `None` (leaving a non-idiom division intact). The
/// accepted cast type is gated to the exact spelling that dialect emits for the idiom — `double
/// precision` on PostgreSQL, `REAL` on SQLite — so a different float cast the renderer never emits for
/// this dialect (e.g. an externally-authored PostgreSQL `CAST(_ AS real)` division) is not peeled and
/// re-rendered with the wrong precision.
fn float_cast_operand(expr: &Expr, dialect: SqlDialect) -> Option<&Expr> {
    let idiom_type = match dialect {
        SqlDialect::Postgres => DataType::DoublePrecision,
        SqlDialect::Sqlite => DataType::Real,
        // MySQL renders a neutral `Divide` bare (no cast); `Generic` is not a round-trip target.
        SqlDialect::Mysql | SqlDialect::Generic => return None,
    };
    match expr {
        Expr::Cast {
            kind: CastKind::Cast,
            expr,
            data_type,
            format: None,
            array: false,
        } if *data_type == idiom_type => Some(expr),
        _ => None,
    }
}

fn lower_function(function: &Function, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    // Only a bare `name(args)` call is a scalar function the renderer emits — no window (`OVER`),
    // `FILTER`, `WITHIN GROUP`, `DISTINCT`, or qualified/parameterized name.
    if function.over.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.parameters != FunctionArguments::None
    {
        return Err(not_yet(format!("function call `{function}`")));
    }
    let name = match function.name.0.as_slice() {
        [part] => part
            .as_ident()
            .map(|ident| ident.value.to_ascii_lowercase()),
        _ => None,
    }
    .ok_or_else(|| not_yet(format!("qualified function name `{}`", function.name)))?;

    let args = match &function.args {
        FunctionArguments::List(list)
            if list.duplicate_treatment.is_none() && list.clauses.is_empty() =>
        {
            list.args
                .iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => lower(expr, dialect),
                    other => Err(not_yet(format!("function argument `{other}`"))),
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        _ => return Err(not_yet(format!("function arguments of `{function}`"))),
    };

    let unary = |func: ScalarFunc, args: Vec<ExprNode>| {
        if args.len() == 1 {
            Ok(ExprNode::ScalarFn { func, args })
        } else {
            Err(not_yet(format!("`{name}` with {} arguments", args.len())))
        }
    };

    match name.as_str() {
        "lower" => unary(ScalarFunc::Lower, args),
        "upper" => unary(ScalarFunc::Upper, args),
        // `CHAR_LENGTH` is character length in every dialect (and is what the renderer emits for
        // `ScalarFunc::Length` on PostgreSQL/MySQL).
        "char_length" => unary(ScalarFunc::Length, args),
        // Bare `length` is character length in SQLite (where the renderer emits it) and in neutral
        // authored SQL (`Generic`). MySQL's `LENGTH` counts *bytes* — folding it to the neutral node
        // (which re-renders as `CHAR_LENGTH`) would silently change semantics on multibyte text, so it is
        // not lowered for MySQL. (PostgreSQL never emits bare `length`; it renders `CHAR_LENGTH`.)
        "length" if matches!(dialect, SqlDialect::Sqlite | SqlDialect::Generic) => {
            unary(ScalarFunc::Length, args)
        }
        "trim" => unary(ScalarFunc::Trim, args),
        // The renderer emits `CONCAT(...)` for `Concat` only on MySQL; PostgreSQL/SQLite use `||`. A
        // `CONCAT(...)` seen on those dialects is externally authored and, on PostgreSQL, has different
        // NULL semantics (it ignores NULLs, whereas the neutral node re-renders as NULL-propagating
        // `||`), so it is only folded for MySQL — and for `Generic`, where either concat spelling denotes
        // the neutral node in authored SQL.
        "concat" if !pipe_is_concatenation(dialect) || dialect == SqlDialect::Generic => {
            Ok(ExprNode::ScalarFn {
                func: ScalarFunc::Concat,
                args,
            })
        }
        _ => Err(not_yet(format!("function `{name}`"))),
    }
}

/// Whether `||` denotes string concatenation in this dialect (mirrors
/// `Dialect::concat_uses_pipe_operator`): PostgreSQL and SQLite (and the permissive `Generic`
/// superset) read `||` as concatenation; MySQL reads it as logical `OR` and renders concatenation as
/// `CONCAT(...)` instead. This gates the two concat spellings so neither is folded on a dialect whose
/// renderer does not emit it.
fn pipe_is_concatenation(dialect: SqlDialect) -> bool {
    matches!(
        dialect,
        SqlDialect::Postgres | SqlDialect::Sqlite | SqlDialect::Generic
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_expr;

    /// Lowers `sql` as the given dialect would have emitted it, panicking on a parse error so a test
    /// asserts against the lowering outcome directly.
    fn low(sql: &str, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
        lower_expr(&parse_expr(sql, dialect).expect("parses"), dialect)
    }

    fn bare(column: &str) -> ExprNode {
        ExprNode::BareColumn {
            column: column.to_owned(),
        }
    }

    fn lit(text: &str) -> ExprNode {
        ExprNode::Literal(text.to_owned())
    }

    #[test]
    fn unqualified_and_qualified_columns() {
        assert_eq!(low("\"sku\"", SqlDialect::Postgres).unwrap(), bare("sku"));
        assert_eq!(
            low("q0_0.\"name\"", SqlDialect::Postgres).unwrap(),
            ExprNode::Column {
                alias: "q0_0".to_owned(),
                column: "name".to_owned(),
            }
        );
    }

    #[test]
    fn unquoted_identifiers_fold_to_lowercase() {
        // An unquoted identifier is case-insensitive (PostgreSQL folds `Id` → `id`); the model stores
        // the folded name so the renderer re-quotes the correct column. A quoted identifier is case-exact.
        assert_eq!(low("Id", SqlDialect::Postgres).unwrap(), bare("id"));
        assert_eq!(low("\"Id\"", SqlDialect::Postgres).unwrap(), bare("Id"));
        assert_eq!(
            low("MixedCase", SqlDialect::Generic).unwrap(),
            bare("mixedcase")
        );
    }

    #[test]
    fn literals_reproduce_their_rendered_text() {
        assert_eq!(low("42", SqlDialect::Postgres).unwrap(), lit("42"));
        assert_eq!(low("-5", SqlDialect::Postgres).unwrap(), lit("-5"));
        assert_eq!(low("1.5", SqlDialect::Postgres).unwrap(), lit("1.5"));
        assert_eq!(low("TRUE", SqlDialect::Postgres).unwrap(), lit("TRUE"));
        assert_eq!(low("FALSE", SqlDialect::Postgres).unwrap(), lit("FALSE"));
        assert_eq!(low("NULL", SqlDialect::Postgres).unwrap(), lit("NULL"));
        // An embedded quote round-trips through the doubled-quote escape the renderer emits.
        assert_eq!(
            low("'it''s'", SqlDialect::Postgres).unwrap(),
            lit("'it''s'")
        );
    }

    #[test]
    fn full_parenthesization_is_stripped() {
        // The renderer wraps every operator node; lowering peels the redundant parens.
        assert_eq!(
            low("(((\"a\" > 1)))", SqlDialect::Postgres).unwrap(),
            ExprNode::Compare {
                op: CompareOp::GreaterThan,
                left: Box::new(bare("a")),
                right: Box::new(lit("1")),
            }
        );
    }

    #[test]
    fn char_length_and_length_fold_to_one_node() {
        let pg = low("CHAR_LENGTH(\"s\")", SqlDialect::Postgres).unwrap();
        let sqlite = low("length(\"s\")", SqlDialect::Sqlite).unwrap();
        let expected = ExprNode::ScalarFn {
            func: ScalarFunc::Length,
            args: vec![bare("s")],
        };
        assert_eq!(pg, expected);
        assert_eq!(sqlite, expected);
    }

    #[test]
    fn concat_pipe_and_function_fold_to_one_node() {
        // `||` (PostgreSQL/SQLite) and `CONCAT(...)` (MySQL) both denote the neutral concat node.
        let pipe = low("(\"a\" || \"b\")", SqlDialect::Postgres).unwrap();
        // MySQL quotes identifiers with backticks (`"a"` there is a string literal, not a column).
        let call = low("CONCAT(`a`, `b`)", SqlDialect::Mysql).unwrap();
        let expected = ExprNode::ScalarFn {
            func: ScalarFunc::Concat,
            args: vec![bare("a"), bare("b")],
        };
        assert_eq!(pipe, expected);
        assert_eq!(call, expected);
    }

    #[test]
    fn substring_from_for_and_comma_forms_fold_to_one_node() {
        let standard = low("SUBSTRING(\"s\" FROM 1 FOR 3)", SqlDialect::Postgres).unwrap();
        let comma = low("substr(\"s\", 1, 3)", SqlDialect::Sqlite).unwrap();
        let expected = ExprNode::ScalarFn {
            func: ScalarFunc::Substring,
            args: vec![bare("s"), lit("1"), lit("3")],
        };
        assert_eq!(standard, expected);
        assert_eq!(comma, expected);
    }

    #[test]
    fn float_cast_division_peels_to_plain_divide() {
        // PostgreSQL `double precision` and SQLite `REAL` casts around `/` are the render idiom for
        // fractional division; both peel back to a bare `Divide` (MySQL renders it with no casts).
        let expected = ExprNode::Binary {
            op: ArithmeticOp::Divide,
            left: Box::new(bare("a")),
            right: Box::new(bare("b")),
        };
        assert_eq!(
            low(
                "(CAST(\"a\" AS double precision) / CAST(\"b\" AS double precision))",
                SqlDialect::Postgres
            )
            .unwrap(),
            expected
        );
        assert_eq!(
            low(
                "(CAST(\"a\" AS REAL) / CAST(\"b\" AS REAL))",
                SqlDialect::Sqlite
            )
            .unwrap(),
            expected
        );
        assert_eq!(low("(`a` / `b`)", SqlDialect::Mysql).unwrap(), expected);
    }

    #[test]
    fn like_ilike_and_negation() {
        assert!(matches!(
            low("(\"n\" LIKE 'a%')", SqlDialect::Postgres).unwrap(),
            ExprNode::Like {
                case_insensitive: false,
                negated: false,
                ..
            }
        ));
        assert!(matches!(
            low("(\"n\" NOT LIKE 'a%')", SqlDialect::Postgres).unwrap(),
            ExprNode::Like {
                case_insensitive: false,
                negated: true,
                ..
            }
        ));
        assert!(matches!(
            low("(\"n\" ILIKE 'a%')", SqlDialect::Postgres).unwrap(),
            ExprNode::Like {
                case_insensitive: true,
                negated: false,
                ..
            }
        ));
    }

    #[test]
    fn is_null_and_not() {
        assert_eq!(
            low("(\"n\" IS NOT NULL)", SqlDialect::Postgres).unwrap(),
            ExprNode::IsNull {
                negated: true,
                operand: Box::new(bare("n"))
            }
        );
        assert_eq!(
            low("(NOT (\"active\"))", SqlDialect::Postgres).unwrap(),
            ExprNode::Not(Box::new(bare("active")))
        );
    }

    #[test]
    fn mysql_byte_length_is_not_folded_to_character_length() {
        // `CHAR_LENGTH` is character length everywhere → folds; MySQL `LENGTH` is *bytes* and must not
        // be folded to the neutral node (which re-renders as `CHAR_LENGTH`).
        assert_eq!(
            low("CHAR_LENGTH(`s`)", SqlDialect::Mysql).unwrap(),
            ExprNode::ScalarFn {
                func: ScalarFunc::Length,
                args: vec![bare("s")],
            }
        );
        assert!(matches!(
            low("LENGTH(`s`)", SqlDialect::Mysql),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn dialect_divergent_spellings_are_not_mislowered() {
        // A bare `/` is fractional (and squealy-emitted) only on MySQL; on PostgreSQL/SQLite it is
        // integer division — folding it would re-render as the float-cast form and change semantics.
        assert_eq!(
            low("(`a` / `b`)", SqlDialect::Mysql).unwrap(),
            ExprNode::Binary {
                op: ArithmeticOp::Divide,
                left: Box::new(bare("a")),
                right: Box::new(bare("b")),
            }
        );
        assert!(matches!(
            low("(\"a\" / \"b\")", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));

        // `||` is concatenation on PostgreSQL/SQLite but logical `OR` on MySQL — never fold it to
        // `Concat` there.
        assert!(!matches!(
            low("(`a` || `b`)", SqlDialect::Mysql),
            Ok(ExprNode::ScalarFn {
                func: ScalarFunc::Concat,
                ..
            })
        ));

        // `CONCAT(...)` is the neutral concat spelling only on MySQL; on PostgreSQL it ignores NULLs
        // (different semantics from the `||` the neutral node re-renders as there).
        assert!(matches!(
            low("CONCAT(\"a\", \"b\")", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn empty_in_sentinels_are_recovered() {
        // The renderer's `<op> IS NOT NULL AND 1 = 0` / `… OR 1 = 1` sentinels round-trip to empty `In`.
        assert_eq!(
            low("(\"status\" IS NOT NULL AND 1 = 0)", SqlDialect::Postgres).unwrap(),
            ExprNode::In {
                negated: false,
                operand: Box::new(bare("status")),
                items: Vec::new(),
            }
        );
        assert_eq!(
            low("(\"status\" IS NOT NULL OR 1 = 1)", SqlDialect::Postgres).unwrap(),
            ExprNode::In {
                negated: true,
                operand: Box::new(bare("status")),
                items: Vec::new(),
            }
        );
        // A genuine `AND`/`OR` that is not the sentinel stays a `Logical`.
        assert!(matches!(
            low("(\"a\" IS NOT NULL AND 1 = 2)", SqlDialect::Postgres).unwrap(),
            ExprNode::Logical { .. }
        ));
    }

    #[test]
    fn generic_is_the_lenient_neutral_authoring_mode() {
        // Under `Generic` (how the derive macro parses an authored check/index string), the neutral
        // spelling of each op lowers directly — `length` is neutral length, bare `/` is neutral divide,
        // and both `||` and `CONCAT(...)` are the neutral concat node.
        assert_eq!(
            low("length(\"s\")", SqlDialect::Generic).unwrap(),
            ExprNode::ScalarFn {
                func: ScalarFunc::Length,
                args: vec![bare("s")],
            }
        );
        assert_eq!(
            low("(\"a\" / \"b\")", SqlDialect::Generic).unwrap(),
            ExprNode::Binary {
                op: ArithmeticOp::Divide,
                left: Box::new(bare("a")),
                right: Box::new(bare("b")),
            }
        );
        let concat = ExprNode::ScalarFn {
            func: ScalarFunc::Concat,
            args: vec![bare("a"), bare("b")],
        };
        assert_eq!(
            low("(\"a\" || \"b\")", SqlDialect::Generic).unwrap(),
            concat
        );
        assert_eq!(
            low("CONCAT(\"a\", \"b\")", SqlDialect::Generic).unwrap(),
            concat
        );
    }

    #[test]
    fn postgres_deparse_idioms_invert_to_the_authored_form() {
        // `pg_get_constraintdef` reshapes a check; introspecting its output must lower to the SAME
        // neutral node the macro produces from the authored (Generic) string, so a published check
        // re-plans to empty. Each pair asserts that convergence.

        // Literal casts: `0` → `(0)::numeric`, `'x'` → `('x')::text`.
        assert_eq!(
            low("(0)::numeric", SqlDialect::Postgres).unwrap(),
            low("0", SqlDialect::Generic).unwrap()
        );
        assert_eq!(
            low("(quota > (0)::numeric)", SqlDialect::Postgres).unwrap(),
            low("quota > 0", SqlDialect::Generic).unwrap()
        );
        // A redundant string→text cast also strips.
        assert_eq!(
            low("('x')::text", SqlDialect::Postgres).unwrap(),
            low("'x'", SqlDialect::Generic).unwrap()
        );
        // A `::` cast on a NON-literal is a real user cast, still not lowered.
        assert!(matches!(
            low("(quota)::numeric", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A CONVERTING literal cast (string → float / date) is meaningful and must NOT be stripped.
        assert!(matches!(
            low("('Infinity')::float8", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low("('2020-01-01')::date", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A fractional literal cast to an INTEGER type truncates (`1.5::integer` = 1) → not redundant.
        assert!(matches!(
            low("(1.5)::integer", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // …but a fractional literal cast to a fractional type is a no-op → strips.
        assert_eq!(
            low("(1.5)::numeric", SqlDialect::Postgres).unwrap(),
            low("1.5", SqlDialect::Generic).unwrap()
        );
        // A LENGTH/PRECISION-bounded cast can truncate/round/pad → not stripped (stays Raw).
        assert!(matches!(
            low("('abcdef')::varchar(3)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low("(1.5)::numeric(2, 0)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A float cast is never provably value-preserving (`(16777217)::real` rounds) → stays Raw.
        assert!(matches!(
            low("(16777217)::real", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low("(1.5)::float8", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));

        // `IN` / `NOT IN` → `= ANY (ARRAY[..])` / `<> ALL (ARRAY[..])`.
        assert_eq!(
            low("(status = ANY (ARRAY[1, 2, 3]))", SqlDialect::Postgres).unwrap(),
            low("status IN (1, 2, 3)", SqlDialect::Generic).unwrap()
        );
        assert_eq!(
            low("(status <> ALL (ARRAY[1, 2]))", SqlDialect::Postgres).unwrap(),
            low("status NOT IN (1, 2)", SqlDialect::Generic).unwrap()
        );

        // `LIKE` / `NOT LIKE` / `ILIKE` → `~~` / `!~~` / `~~*`.
        assert_eq!(
            low("(name ~~ 'a%')", SqlDialect::Postgres).unwrap(),
            low("name LIKE 'a%'", SqlDialect::Generic).unwrap()
        );
        assert_eq!(
            low("(name !~~ 'a%')", SqlDialect::Postgres).unwrap(),
            low("name NOT LIKE 'a%'", SqlDialect::Generic).unwrap()
        );
        assert!(matches!(
            low("(name ~~* 'a%')", SqlDialect::Postgres).unwrap(),
            ExprNode::Like {
                case_insensitive: true,
                ..
            }
        ));
    }

    #[test]
    fn shapes_outside_the_grammar_are_not_yet_lowered() {
        // `%` has no neutral arithmetic node.
        assert!(matches!(
            low("(\"a\" % 2)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A general `CAST` is deferred (dialect-ambiguous target names, e.g. MySQL `SIGNED`).
        assert!(matches!(
            low("CAST(\"a\" AS integer)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A division whose float casts are NOT the dialect's idiom type (PostgreSQL emits `double
        // precision`, not `real`) is external, not the render idiom → not peeled/lowered.
        assert!(matches!(
            low(
                "(CAST(\"a\" AS real) / CAST(\"b\" AS real))",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        // PostgreSQL's regex `SUBSTRING(s FROM 'pattern' FOR 'escape')` overload (string bounds) is a
        // different operation from positional substring → not lowered.
        assert!(matches!(
            low("SUBSTRING(\"s\" FROM 'a.*' FOR '#')", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A general/user function outside the closed scalar set.
        assert!(matches!(
            low("md5(\"s\")", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A subquery in a scalar position.
        assert!(matches!(
            low("(\"a\" IN (SELECT 1))", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A windowed call is not a scalar constraint function.
        assert!(matches!(
            low("ROW_NUMBER() OVER ()", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
    }
}
