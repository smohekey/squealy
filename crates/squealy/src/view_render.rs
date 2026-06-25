//! Dialect-driven rendering of view DDL from the neutral [`ViewModel`].
//!
//! View bodies are stored structurally ([`ViewQueryModel`]/[`ExprNode`]); this module renders them to
//! SQL for a given [`Dialect`], so each backend gets dialect-correct identifier quoting, cast type
//! names, integer-division casts, and `LIKE`/`ILIKE` from one shared renderer. Backends call
//! [`render_create_view`]/[`render_drop_view`] (and [`ordered_views`] for create-from-scratch order).

use std::io::{self, Write};

use crate::{
    AggregateFunc, ArithmeticOp, DatabaseModel, Dialect, ExprNode, JoinKind, LogicalOp,
    OrderDirection, ScalarFunc, SourceRef, SqlType, ViewModel, ViewQueryModel, WindowFunc,
};

/// Renders `CREATE [OR REPLACE] VIEW <qualified> [(<cols>)] AS <select>` for the given dialect.
pub fn render_create_view(
    schema: Option<&str>,
    view: &ViewModel,
    or_replace: bool,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    // A view with no projection cannot render a valid `SELECT`. The only models that carry such a body
    // are live-introspected ones (whose definition could not be reconstructed into the structural IR);
    // they exist to diff against, not to materialize. Fail clearly rather than emit `AS SELECT` with no
    // output, which would turn a database containing views into an unusable package/plan.
    if view.query.projection.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cannot render view `{}`: its body has no projection — an introspected view (whose \
                 definition could not be reconstructed) cannot be rendered to DDL",
                view.name
            ),
        ));
    }

    writer.write_all(if or_replace {
        b"CREATE OR REPLACE VIEW ".as_slice()
    } else {
        b"CREATE VIEW ".as_slice()
    })?;
    render_qualified(schema, &view.name, dialect, writer)?;

    // The declared columns name the view's outputs positionally; without them, fall back to aliasing
    // each projected expression (hand-built models with no declared columns).
    if !view.columns.is_empty() {
        writer.write_all(b" (")?;
        for (index, column) in view.columns.iter().enumerate() {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            dialect.write_quoted_ident(&column.name, writer)?;
        }
        writer.write_all(b")")?;
    }

    writer.write_all(b" AS ")?;
    render_select(&view.query, view.columns.is_empty(), dialect, writer)
}

/// Renders `DROP VIEW <qualified>`.
pub fn render_drop_view(
    schema: Option<&str>,
    name: &str,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    writer.write_all(b"DROP VIEW ")?;
    render_qualified(schema, name, dialect, writer)
}

/// Every view in `model` in dependency order — a view after every other view it selects from — so a
/// view-on-view never references a sibling that does not exist yet. A depth-first post-order keyed by
/// `(schema, name)`; reference cycles (which SQL rejects) fall back to declaration order.
pub fn ordered_views(model: &DatabaseModel) -> Vec<(Option<&str>, &ViewModel)> {
    let views: Vec<(Option<&str>, &ViewModel)> = model
        .schemas
        .iter()
        .flat_map(|schema| {
            schema
                .views
                .iter()
                .map(move |view| (schema.name.as_deref(), view))
        })
        .collect();

    let index_of = |schema: Option<&str>, name: &str| {
        views
            .iter()
            .position(|(s, v)| *s == schema && v.name == name)
    };

    fn visit<'a>(
        current: usize,
        views: &[(Option<&'a str>, &'a ViewModel)],
        visited: &mut [bool],
        ordered: &mut Vec<(Option<&'a str>, &'a ViewModel)>,
        index_of: &impl Fn(Option<&str>, &str) -> Option<usize>,
    ) {
        if visited[current] {
            return;
        }
        visited[current] = true;
        let (schema, view) = views[current];
        for source in view.referenced_sources() {
            let dep_schema = source.schema.as_deref().or(schema);
            if let Some(dep) = index_of(dep_schema, &source.name)
                && dep != current
            {
                visit(dep, views, visited, ordered, index_of);
            }
        }
        ordered.push((schema, view));
    }

    let mut ordered = Vec::with_capacity(views.len());
    let mut visited = vec![false; views.len()];
    for current in 0..views.len() {
        visit(current, &views, &mut visited, &mut ordered, &index_of);
    }
    ordered
}

fn render_qualified(
    schema: Option<&str>,
    name: &str,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    if let Some(schema) = schema {
        dialect.write_quoted_ident(schema, writer)?;
        writer.write_all(b".")?;
    }
    dialect.write_quoted_ident(name, writer)
}

/// Renders the `SELECT …` body. `alias_projections` emits `AS <name>` per projected expression (used
/// for subqueries and column-less views); otherwise the enclosing `CREATE VIEW (<cols>)` names them.
fn render_select(
    query: &ViewQueryModel,
    alias_projections: bool,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    writer.write_all(b"SELECT ")?;
    if query.distinct {
        writer.write_all(b"DISTINCT ")?;
    }
    for (index, item) in query.projection.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        render_expr(&item.expr, dialect, writer)?;
        if alias_projections {
            writer.write_all(b" AS ")?;
            dialect.write_quoted_ident(&item.output_name, writer)?;
        }
    }

    if let Some(from) = &query.from {
        writer.write_all(b" FROM ")?;
        render_source(from, dialect, writer)?;
    }

    for join in &query.joins {
        writer.write_all(match join.kind {
            JoinKind::Inner => b" INNER JOIN ".as_slice(),
            JoinKind::Left => b" LEFT JOIN ".as_slice(),
            JoinKind::Right => b" RIGHT JOIN ".as_slice(),
            JoinKind::Full => b" FULL JOIN ".as_slice(),
        })?;
        render_source(&join.source, dialect, writer)?;
        writer.write_all(b" ON ")?;
        render_expr(&join.on, dialect, writer)?;
    }

    if let Some(filter) = &query.filter {
        writer.write_all(b" WHERE ")?;
        render_expr(filter, dialect, writer)?;
    }

    for (index, key) in query.group_by.iter().enumerate() {
        writer.write_all(if index == 0 { b" GROUP BY " } else { b", " })?;
        render_expr(key, dialect, writer)?;
    }

    if let Some(having) = &query.having {
        writer.write_all(b" HAVING ")?;
        render_expr(having, dialect, writer)?;
    }

    for (index, order) in query.order_by.iter().enumerate() {
        writer.write_all(if index == 0 { b" ORDER BY " } else { b", " })?;
        render_expr(&order.expr, dialect, writer)?;
        match order.direction {
            Some(OrderDirection::Asc) => writer.write_all(b" ASC")?,
            Some(OrderDirection::Desc) => writer.write_all(b" DESC")?,
            None => {}
        }
        if let Some(nulls) = order.nulls {
            dialect.write_order_nulls(nulls, writer)?;
        }
    }

    dialect.write_limit_offset(query.limit, query.offset, writer)
}

/// Writes a `<qualified> AS <alias>` source. The alias is emitted unquoted so it matches the column
/// references inside expressions, which qualify with the bare alias.
fn render_source(
    source: &SourceRef,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    render_qualified(source.schema.as_deref(), &source.name, dialect, writer)?;
    write!(writer, " AS {}", source.alias)
}

fn render_expr(node: &ExprNode, dialect: &dyn Dialect, writer: &mut dyn Write) -> io::Result<()> {
    match node {
        ExprNode::Column { alias, column } => {
            write!(writer, "{alias}.")?;
            dialect.write_quoted_ident(column, writer)
        }
        ExprNode::Literal(text) => writer.write_all(text.as_bytes()),
        ExprNode::Binary { op, left, right } => {
            if *op == ArithmeticOp::Divide && dialect.integer_division_needs_float_cast() {
                // Cast operands to float so integer `/` matches the builder's always-fractional
                // division; dialects whose `/` is already float (MySQL) skip this.
                writer.write_all(b"(CAST(")?;
                render_expr(left, dialect, writer)?;
                writer.write_all(b" AS ")?;
                dialect.write_cast_type(&SqlType::F64, writer)?;
                writer.write_all(b") / CAST(")?;
                render_expr(right, dialect, writer)?;
                writer.write_all(b" AS ")?;
                dialect.write_cast_type(&SqlType::F64, writer)?;
                writer.write_all(b"))")
            } else {
                writer.write_all(b"(")?;
                render_expr(left, dialect, writer)?;
                write!(writer, " {} ", arithmetic_symbol(*op))?;
                render_expr(right, dialect, writer)?;
                writer.write_all(b")")
            }
        }
        ExprNode::Cast { operand, ty } => {
            writer.write_all(b"CAST(")?;
            render_expr(operand, dialect, writer)?;
            writer.write_all(b" AS ")?;
            dialect.write_cast_type(ty, writer)?;
            writer.write_all(b")")
        }
        ExprNode::Aggregate {
            func,
            distinct,
            operand,
            result,
        } => {
            if result.is_some() {
                writer.write_all(b"CAST(")?;
            }
            write!(writer, "{}(", aggregate_name(*func))?;
            if *distinct {
                writer.write_all(b"DISTINCT ")?;
            }
            render_expr(operand, dialect, writer)?;
            writer.write_all(b")")?;
            if let Some(ty) = result {
                writer.write_all(b" AS ")?;
                dialect.write_cast_type(ty, writer)?;
                writer.write_all(b")")?;
            }
            Ok(())
        }
        ExprNode::Compare { op, left, right } => {
            writer.write_all(b"(")?;
            render_expr(left, dialect, writer)?;
            write!(writer, " {} ", crate::render::render_compare_op(*op))?;
            render_expr(right, dialect, writer)?;
            writer.write_all(b")")
        }
        ExprNode::Logical { op, left, right } => {
            writer.write_all(b"(")?;
            render_expr(left, dialect, writer)?;
            writer.write_all(match op {
                LogicalOp::And => b" AND ".as_slice(),
                LogicalOp::Or => b" OR ".as_slice(),
            })?;
            render_expr(right, dialect, writer)?;
            writer.write_all(b")")
        }
        ExprNode::Not(operand) => {
            writer.write_all(b"(NOT ")?;
            render_expr(operand, dialect, writer)?;
            writer.write_all(b")")
        }
        ExprNode::IsNull { negated, operand } => {
            writer.write_all(b"(")?;
            render_expr(operand, dialect, writer)?;
            writer.write_all(if *negated {
                b" IS NOT NULL)".as_slice()
            } else {
                b" IS NULL)".as_slice()
            })
        }
        ExprNode::Like {
            case_insensitive,
            negated,
            operand,
            pattern,
        } => {
            writer.write_all(b"(")?;
            render_expr(operand, dialect, writer)?;
            dialect.write_like_operator(*case_insensitive, *negated, writer)?;
            render_expr(pattern, dialect, writer)?;
            writer.write_all(b")")
        }
        ExprNode::In {
            negated,
            operand,
            items,
        } => {
            writer.write_all(b"(")?;
            render_expr(operand, dialect, writer)?;
            if items.is_empty() {
                // SQL has no `IN ()`; fix the truth value with a constant.
                return writer.write_all(if *negated {
                    b" IS NOT NULL OR 1 = 1)".as_slice()
                } else {
                    b" IS NOT NULL AND 1 = 0)".as_slice()
                });
            }
            writer.write_all(if *negated {
                b" NOT IN (".as_slice()
            } else {
                b" IN (".as_slice()
            })?;
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b", ")?;
                }
                render_expr(item, dialect, writer)?;
            }
            writer.write_all(b"))")
        }
        ExprNode::Between {
            negated,
            operand,
            low,
            high,
        } => {
            writer.write_all(b"(")?;
            render_expr(operand, dialect, writer)?;
            writer.write_all(if *negated {
                b" NOT BETWEEN ".as_slice()
            } else {
                b" BETWEEN ".as_slice()
            })?;
            render_expr(low, dialect, writer)?;
            writer.write_all(b" AND ")?;
            render_expr(high, dialect, writer)?;
            writer.write_all(b")")
        }
        ExprNode::ScalarSubquery(subquery) => {
            writer.write_all(b"(")?;
            render_select(subquery, true, dialect, writer)?;
            writer.write_all(b")")
        }
        ExprNode::InSubquery {
            negated,
            operand,
            subquery,
        } => {
            writer.write_all(b"(")?;
            render_expr(operand, dialect, writer)?;
            writer.write_all(if *negated {
                b" NOT IN (".as_slice()
            } else {
                b" IN (".as_slice()
            })?;
            render_select(subquery, true, dialect, writer)?;
            writer.write_all(b"))")
        }
        ExprNode::Exists { negated, subquery } => {
            writer.write_all(if *negated {
                b"(NOT EXISTS (".as_slice()
            } else {
                b"(EXISTS (".as_slice()
            })?;
            render_select(subquery, true, dialect, writer)?;
            writer.write_all(b"))")
        }
        ExprNode::Window {
            func,
            args,
            partition_by,
            order_by,
            result,
        } => {
            if result.is_some() {
                writer.write_all(b"CAST(")?;
            }
            write!(writer, "{}(", window_func_name(*func))?;
            for (index, arg) in args.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b", ")?;
                }
                render_expr(arg, dialect, writer)?;
            }
            writer.write_all(b") OVER (")?;
            if !partition_by.is_empty() {
                writer.write_all(b"PARTITION BY ")?;
                for (index, partition) in partition_by.iter().enumerate() {
                    if index > 0 {
                        writer.write_all(b", ")?;
                    }
                    render_expr(partition, dialect, writer)?;
                }
            }
            if !order_by.is_empty() {
                if !partition_by.is_empty() {
                    writer.write_all(b" ")?;
                }
                writer.write_all(b"ORDER BY ")?;
                for (index, order) in order_by.iter().enumerate() {
                    if index > 0 {
                        writer.write_all(b", ")?;
                    }
                    render_expr(&order.expr, dialect, writer)?;
                    writer.write_all(match order.direction {
                        OrderDirection::Asc => b" ASC".as_slice(),
                        OrderDirection::Desc => b" DESC".as_slice(),
                    })?;
                }
            }
            writer.write_all(b")")?;
            if let Some(ty) = result {
                writer.write_all(b" AS ")?;
                dialect.write_cast_type(ty, writer)?;
                writer.write_all(b")")?;
            }
            Ok(())
        }
        ExprNode::Case {
            arms,
            else_,
            result,
        } => {
            // Each branch value is wrapped in `CAST(… AS result)` (not the whole `CASE`) so an
            // all-parameter branch is typeable; mirrors the query renderer.
            writer.write_all(b"CASE")?;
            for arm in arms {
                writer.write_all(b" WHEN ")?;
                render_expr(&arm.when, dialect, writer)?;
                writer.write_all(b" THEN ")?;
                render_case_value(&arm.then, result.as_ref(), dialect, writer)?;
            }
            if let Some(else_) = else_ {
                writer.write_all(b" ELSE ")?;
                render_case_value(else_, result.as_ref(), dialect, writer)?;
            }
            writer.write_all(b" END")
        }
        ExprNode::Nullif {
            left,
            right,
            result,
        } => {
            // Cast only when both operands are inlined literals (no typed column to anchor the type);
            // otherwise a column anchors the other and neither is cast, preserving its type/collation.
            let cast = if is_literal(left) && is_literal(right) {
                result.as_ref()
            } else {
                None
            };
            writer.write_all(b"NULLIF(")?;
            render_case_value(left, cast, dialect, writer)?;
            writer.write_all(b", ")?;
            render_case_value(right, cast, dialect, writer)?;
            writer.write_all(b")")
        }
        ExprNode::Coalesce { args, result } => {
            // Cast only when every argument is an inlined literal (no typed column to anchor the result
            // type); otherwise a column anchors them and none are cast, preserving its type/collation.
            let cast = if args.iter().all(is_literal) {
                result.as_ref()
            } else {
                None
            };
            writer.write_all(b"COALESCE(")?;
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    writer.write_all(b", ")?;
                }
                render_case_value(arg, cast, dialect, writer)?;
            }
            writer.write_all(b")")
        }
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            result,
        } => {
            writer.write_all(b"CASE ")?;
            render_expr(operand, dialect, writer)?;
            for arm in arms {
                writer.write_all(b" WHEN ")?;
                render_expr(&arm.when, dialect, writer)?;
                writer.write_all(b" THEN ")?;
                render_case_value(&arm.then, result.as_ref(), dialect, writer)?;
            }
            if let Some(else_) = else_ {
                writer.write_all(b" ELSE ")?;
                render_case_value(else_, result.as_ref(), dialect, writer)?;
            }
            writer.write_all(b" END")
        }
        ExprNode::ScalarFn { func, args } => match func {
            // `CONCAT` ignores NULL on PostgreSQL, so render concat there as `||` (NULL-propagating),
            // matching the builder's nullability model.
            ScalarFunc::Concat if dialect.concat_uses_pipe_operator() => {
                writer.write_all(b"(")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        writer.write_all(b" || ")?;
                    }
                    render_expr(arg, dialect, writer)?;
                }
                writer.write_all(b")")
            }
            // The SQL-standard `SUBSTRING(s FROM start FOR len)` form (unambiguous; the comma form can
            // resolve to PostgreSQL's regex overload).
            ScalarFunc::Substring if args.len() == 3 => {
                writer.write_all(b"SUBSTRING(")?;
                render_expr(&args[0], dialect, writer)?;
                writer.write_all(b" FROM ")?;
                render_expr(&args[1], dialect, writer)?;
                writer.write_all(b" FOR ")?;
                render_expr(&args[2], dialect, writer)?;
                writer.write_all(b")")
            }
            _ => {
                writer.write_all(scalar_func_name(*func).as_bytes())?;
                writer.write_all(b"(")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        writer.write_all(b", ")?;
                    }
                    render_expr(arg, dialect, writer)?;
                }
                writer.write_all(b")")
            }
        },
    }
}

/// SQL name for a [`ScalarFunc`] (identical across backends; `Length` -> `CHAR_LENGTH`).
fn scalar_func_name(func: ScalarFunc) -> &'static str {
    match func {
        ScalarFunc::Lower => "LOWER",
        ScalarFunc::Upper => "UPPER",
        ScalarFunc::Length => "CHAR_LENGTH",
        ScalarFunc::Trim => "TRIM",
        ScalarFunc::Concat => "CONCAT",
        ScalarFunc::Substring => "SUBSTRING",
    }
}

/// An inlined SQL literal — the only `NULLIF`/`COALESCE` operand kind that has no inherent type (a
/// column/expression carries its own). When every operand of such a node is a literal there is no
/// typed operand to anchor the type, so the literals are cast; otherwise a column anchors them and they
/// keep their own type/collation (e.g. a `citext` column's case-insensitivity). View bodies inline
/// literals, so a runtime param never appears here.
fn is_literal(node: &ExprNode) -> bool {
    matches!(node, ExprNode::Literal(_))
}

/// Renders a `CASE` branch value, wrapping it in `CAST(… AS <cast>)` when a result cast is set.
fn render_case_value(
    value: &ExprNode,
    cast: Option<&SqlType>,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    match cast {
        Some(ty) => {
            writer.write_all(b"CAST(")?;
            render_expr(value, dialect, writer)?;
            writer.write_all(b" AS ")?;
            dialect.write_cast_type(ty, writer)?;
            writer.write_all(b")")
        }
        None => render_expr(value, dialect, writer),
    }
}

fn window_func_name(func: WindowFunc) -> &'static str {
    match func {
        WindowFunc::Aggregate(aggregate) => aggregate_name(aggregate),
        WindowFunc::RowNumber => "ROW_NUMBER",
        WindowFunc::Rank => "RANK",
        WindowFunc::DenseRank => "DENSE_RANK",
        WindowFunc::Ntile => "NTILE",
        WindowFunc::Lag => "LAG",
        WindowFunc::Lead => "LEAD",
    }
}

fn arithmetic_symbol(op: ArithmeticOp) -> &'static str {
    match op {
        ArithmeticOp::Add => "+",
        ArithmeticOp::Subtract => "-",
        ArithmeticOp::Multiply => "*",
        ArithmeticOp::Divide => "/",
    }
}

fn aggregate_name(func: AggregateFunc) -> &'static str {
    match func {
        AggregateFunc::Count => "COUNT",
        AggregateFunc::Sum => "SUM",
        AggregateFunc::Avg => "AVG",
        AggregateFunc::Min => "MIN",
        AggregateFunc::Max => "MAX",
    }
}
