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
//! `AND`/`OR`/`NOT`, `IS [NOT] NULL`, `IN (<list>)`, `BETWEEN`, `LIKE`/`ILIKE`, the closed
//! scalar-function set (`LOWER`/`UPPER`/`CHAR_LENGTH`/`TRIM`/`CONCAT`/`SUBSTRING`), and any other
//! *unquoted*-named function with no direct literal argument as a general, dialect-neutral
//! [`ExprNode::Function`] call (`jsonb_typeof(data)`).
//!
//! It additionally covers the **view-body** node set the SELECT renderer emits: aggregates
//! (`FUNC([DISTINCT] x)`, peeling the outer `CAST(<agg> AS ty)` result-pin into `result`), `CASE` /
//! simple `CASE` / `NULLIF` / `COALESCE` (recovering the per-branch `CAST(… AS ty)` casts into
//! `result`), `CURRENT_TIMESTAMP[(n)]` → `Now`, `EXTRACT(<field> FROM …)` (with the `Second` `FLOOR`,
//! the `AT TIME ZONE` operand, and the outer cast-pin), `date_trunc('unit', …)`, window functions
//! (`FUNC(args) OVER (PARTITION BY … ORDER BY …)`), and scalar / `IN` / `EXISTS` subqueries (recursing
//! through [`lower_query`]).
//!
//! [`lower_query`] reconstructs a single-`SELECT` view body into a [`ViewQueryModel`]: `DISTINCT`,
//! projection, one named `FROM` source, `INNER`/`LEFT`/`RIGHT`/`FULL`/`CROSS` joins, `WHERE`,
//! `GROUP BY`, `HAVING`, `ORDER BY`, and integer-literal `LIMIT`/`OFFSET`.
//!
//! Remaining shapes outside these (`%` modulo — no neutral node; a general `CAST` — dialect-ambiguous
//! target names; a *quoted* function name or a function with a *direct literal argument*; a `CAST` pin
//! whose target type this dialect's cast vocabulary cannot invert to an exact [`SqlType`]; a window
//! `FILTER`/frame; set operations / CTEs / derived tables / comma joins; `USING`/`NATURAL` joins;
//! non-integer `LIMIT`) yield [`ReadError::NotYetLowered`]. A query-/select-level clause the
//! [`ViewQueryModel`] cannot hold (`FETCH`, `FOR UPDATE`, `QUALIFY`, …) is rejected up front rather than
//! silently dropped (`reject_unsupported_clauses`).
//!
//! One shape is unreachable, not merely un-lowered: MySQL renders [`ExprNode::ExtractSecond`] as the
//! composite `CAST(EXTRACT(SECOND_MICROSECOND FROM x) / 1000000.0 AS …)`, but `sqlparser` 0.62's MySQL
//! dialect does not accept `SECOND_MICROSECOND` as an `EXTRACT` field, so that SQL fails at the *parse*
//! step (before lowering). A MySQL view body using `extract_second()` therefore cannot round-trip yet —
//! a parser limitation, tracked separately, not a lowering gap.

use sqlparser::ast::{
    BinaryOperator, CaseWhen, CastKind, CeilFloorKind, CreateTableOptions, CreateView, DataType,
    DateTimeField, Distinct, DuplicateTreatment, Expr, ExtractSyntax, Function, FunctionArg,
    FunctionArgExpr, FunctionArguments, GroupByExpr, Join, JoinConstraint, JoinOperator,
    LimitClause, ObjectName, OrderBy, OrderByExpr, OrderByKind, Query, Select, SelectFlavor,
    SelectItem, SetExpr, SetOperator, SetQuantifier, TableAlias, TableFactor, UnaryOperator, Value,
    WindowType,
};
use squealy_ir::{
    AggregateFunc, ArithmeticOp, CaseArm, CompareOp, CteModel, DateField, ExprNode, JoinItem,
    JoinKind, LogicalOp, OrderDirection, OrderItem, OrderNulls, ProjectionItem, ScalarFunc,
    SourceItem, SourceRef, SqlType, ViewBody, ViewQueryModel, ViewSetOp, WindowFunc,
    WindowOrderTerm,
};

use crate::{ReadError, SqlDialect};

/// Lowers a parsed scalar expression into an [`ExprNode`].
///
/// Handles the grammar the renderer emits for a `CHECK` / generated-column / index expression (see the
/// [module docs](self)); shapes outside it return [`ReadError::NotYetLowered`] naming the offending
/// node, so a caller (the round-trip harness, a macro, live introspection) sees exactly what remains.
pub fn lower_expr(expr: &Expr, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    lower(expr, dialect)
}

/// Lowers a parsed single-`SELECT` [`Query`] (a view body, or the deparse a backend returns for a
/// view definition) into a [`ViewQueryModel`] — the structural inverse of the SELECT renderer.
///
/// Projection output names are taken from the projection aliases (`… AS name`), the form the renderer
/// emits for a subquery or a column-less view; a bare-column projection is named by its column. A
/// column-listed `CREATE VIEW (cols) AS …` names its (un-aliased) projections from the column list
/// instead — see [`lower_create_view`], which supplies them.
///
/// Shapes outside the single-SELECT grammar — a `WITH` clause, a set operation
/// (`UNION`/`EXCEPT`/`INTERSECT`), a `VALUES`, a derived-table (subquery) `FROM`, comma joins
/// (multiple `FROM` entries), `USING`/`NATURAL` joins, a wildcard projection, or a non-integer
/// `LIMIT`/`OFFSET` — return [`ReadError::NotYetLowered`] (they land in later phases).
pub fn lower_query(query: &Query, dialect: SqlDialect) -> Result<ViewBody, ReadError> {
    let mut body = lower_body(query, None, false, dialect)?;
    resolve_single_source_columns(&mut body);
    Ok(body)
}

/// Lowers a parsed `CREATE VIEW` into its body [`ViewBody`]. When the statement carries a declared column
/// list (`CREATE VIEW v (a, b) AS …`), the renderer leaves the projections un-aliased and those names
/// identify the outputs; they are supplied here so each projection is named positionally. The view's
/// output column *types* are not present in the SQL text (only names), so this returns just the body —
/// the backend supplies the typed columns from its catalog.
pub fn lower_create_view(
    create_view: &CreateView,
    dialect: SqlDialect,
) -> Result<ViewBody, ReadError> {
    // The renderer emits only `CREATE [OR REPLACE] VIEW <name> [(cols)] AS <select>`. A modifier it never
    // emits (`MATERIALIZED`, `TEMPORARY`, `SECURE`, `IF NOT EXISTS`, `WITH (…)` options, `CLUSTER BY`, a
    // comment, …) carries semantics the body-only `ViewBody` cannot hold — reject it, else a caller
    // re-rendering the body as a plain `CREATE VIEW` would silently drop those semantics.
    reject_unsupported_view_modifiers(create_view)?;

    // The declared column names (`fold_ident` so a quoted name stays case-exact) name the outputs
    // positionally; an empty list means the projections carry their own `AS` aliases instead. For a
    // set-op body the list names the *whole set's* outputs, which SQL takes from the leftmost `SELECT`.
    let names: Vec<String> = create_view
        .columns
        .iter()
        .map(|column| fold_ident(&column.name))
        .collect();
    let names = if names.is_empty() {
        None
    } else {
        Some(names.as_slice())
    };
    let mut body = lower_body(&create_view.query, names, false, dialect)?;
    resolve_single_source_columns(&mut body);
    Ok(body)
}

/// The shared query-lowering core: a full [`Query`] (its `WITH`/`ORDER BY`/`LIMIT` plus a `SetExpr` body)
/// into a [`ViewBody`]. `output_names`, when `Some`, is a declared column list (a `CREATE VIEW (cols)` or
/// `WITH cte (cols)` list) naming the outputs positionally. `column_list_is_fallback` distinguishes how it
/// applies to a set operation's arms (see [`lower_select`]): `false` for a body whose column list is
/// authoritative; `true` when this body is itself a set-operation arm (the list only names an arm's
/// otherwise-unnamed projection). A single `SELECT` becomes [`ViewBody::Select`] (its query-level
/// `ORDER BY`/`LIMIT` attach to that select); a set operation becomes [`ViewBody::Set`] with the
/// query-level `ORDER BY`/`LIMIT` as the trailing whole-set tail; a `WITH` prelude becomes [`ViewBody::With`].
fn lower_body(
    query: &Query,
    output_names: Option<&[String]>,
    column_list_is_fallback: bool,
    dialect: SqlDialect,
) -> Result<ViewBody, ReadError> {
    // A `WITH` (CTE) prelude wraps the query's `SELECT`/`Set` body in a [`ViewBody::With`]. The prelude's
    // own trailing `ORDER BY`/`LIMIT` belong to that inner body, so the inner body is lowered from the
    // very same `query` (minus the `WITH`) below.
    if let Some(with) = query.with.as_ref() {
        let ctes = lower_ctes(with, dialect)?;
        let body = lower_query_body(query, output_names, column_list_is_fallback, dialect)?;
        return Ok(ViewBody::With {
            recursive: with.recursive,
            ctes,
            body: Box::new(body),
        });
    }
    lower_query_body(query, output_names, column_list_is_fallback, dialect)
}

/// Lowers each CTE of a `WITH` prelude into a [`CteModel`]. A CTE's own body is a full [`Query`], lowered
/// recursively (a recursive CTE's body is a `UNION` set whose recursive arm references the CTE name — no
/// special handling here; it lowers like any set). `MATERIALIZED`/`NOT MATERIALIZED` hints and PartiQL
/// `FROM`/`AT` alias extensions carry semantics the neutral model does not model — reject rather than drop.
fn lower_ctes(
    with: &sqlparser::ast::With,
    dialect: SqlDialect,
) -> Result<Vec<CteModel>, ReadError> {
    with.cte_tables
        .iter()
        .map(|cte| {
            if cte.materialized.is_some() {
                return Err(not_yet("MATERIALIZED / NOT MATERIALIZED CTE"));
            }
            if cte.from.is_some() {
                return Err(not_yet("CTE with a `FROM` identifier (PartiQL)"));
            }
            if cte.alias.at.is_some() {
                return Err(not_yet("CTE alias with an `AT` index (PartiQL)"));
            }
            // Fold the CTE name and its column aliases the same way sources/projection aliases are folded
            // (`fold_ident`), so an unquoted mixed-case CTE (`WITH Recent(Id) …`) is stored under the same
            // folded name its references resolve to — else re-rendering would quote a name the body no
            // longer binds to.
            let name = fold_ident(&cte.alias.name);
            let columns: Vec<String> = cte
                .alias
                .columns
                .iter()
                .map(|column| fold_ident(&column.name))
                .collect();
            // A declared CTE column list names the body's outputs positionally — exactly like a
            // column-listed `CREATE VIEW` (authoritative, not a set-arm fallback). Without it, a
            // column-listed CTE whose body has an unaliased projection (which the renderer emits, since the
            // list names the outputs) — including the un-aliased anchor of a recursive CTE — would fail to
            // lower.
            let output_names = (!columns.is_empty()).then_some(columns.as_slice());
            let body = lower_body(&cte.query, output_names, false, dialect)?;
            Ok(CteModel {
                name,
                columns,
                body,
            })
        })
        .collect()
}

/// The query-body core (a full [`Query`]'s `SetExpr` body plus its trailing `ORDER BY`/`LIMIT`), without
/// the `WITH` prelude [`lower_body`] handles. See [`lower_body`] for the `output_names` semantics.
fn lower_query_body(
    query: &Query,
    output_names: Option<&[String]>,
    column_list_is_fallback: bool,
    dialect: SqlDialect,
) -> Result<ViewBody, ReadError> {
    reject_unsupported_query_clauses(query)?;
    match query.body.as_ref() {
        SetExpr::Select(select) => {
            let mut model = lower_select(select, output_names, column_list_is_fallback, dialect)?;
            // A single `SELECT`'s query-level `ORDER BY`/`LIMIT`/`OFFSET` belong to that select.
            model.order_by = lower_order_by(query.order_by.as_ref(), dialect)?;
            (model.limit, model.offset) = lower_limit_offset(query.limit_clause.as_ref(), dialect)?;
            Ok(ViewBody::Select(Box::new(model)))
        }
        SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } => {
            let (set_op, all) = lower_set_op(op, set_quantifier)?;
            // A column list names the compound output (also recorded on `ViewModel.columns`), which SQL
            // takes from the arms positionally. Each arm keeps its own projection aliases for its internal
            // clauses (a per-arm `ORDER BY <alias>`), so the list is only a FALLBACK naming an arm's
            // otherwise-unnamed projection — e.g. a recursive CTE's un-aliased anchor `SELECT 1`. Passed to
            // both arms (either may be un-aliased); `lower_set_operand` lowers arms with fallback semantics.
            let left = lower_set_operand(left, output_names, dialect)?;
            let right = lower_set_operand(right, output_names, dialect)?;
            // The query-level `ORDER BY`/`LIMIT` here apply to the whole set (after the final arm).
            let order_by = lower_order_by(query.order_by.as_ref(), dialect)?;
            let (limit, offset) = lower_limit_offset(query.limit_clause.as_ref(), dialect)?;
            Ok(ViewBody::Set {
                op: set_op,
                all,
                left: Box::new(left),
                right: Box::new(right),
                order_by,
                limit,
                offset,
            })
        }
        // A parenthesized top-level body (`(SELECT …) ORDER BY … LIMIT …` or `(… UNION …) ORDER BY …`).
        // The parser attaches the trailing `ORDER BY`/`LIMIT` to *this* outer query, so fold it onto the
        // lowered inner body rather than dropping it (which would re-render a different result set).
        SetExpr::Query(inner) => {
            let body = lower_body(inner, output_names, column_list_is_fallback, dialect)?;
            apply_query_tail(body, query, dialect)
        }
        SetExpr::Values(_) => Err(not_yet("VALUES body")),
        other => Err(not_yet(format!("non-SELECT query body `{other}`"))),
    }
}

/// Applies a query's trailing `ORDER BY`/`LIMIT`/`OFFSET` to an already-lowered [`ViewBody`] — the outer
/// tail of a parenthesized body (`(…) ORDER BY … LIMIT …`), which the parser hangs on the outer query.
/// A no-op when there is no tail. If the inner body already carries a tail (two nested tails, which the
/// model cannot represent) this is [`ReadError::NotYetLowered`] rather than a silent drop of one level.
fn apply_query_tail(
    body: ViewBody,
    query: &Query,
    dialect: SqlDialect,
) -> Result<ViewBody, ReadError> {
    let order_by = lower_order_by(query.order_by.as_ref(), dialect)?;
    let (limit, offset) = lower_limit_offset(query.limit_clause.as_ref(), dialect)?;
    if order_by.is_empty() && limit.is_none() && offset.is_none() {
        return Ok(body);
    }
    match body {
        ViewBody::Select(mut select) => {
            if !select.order_by.is_empty() || select.limit.is_some() || select.offset.is_some() {
                return Err(not_yet("nested ORDER BY/LIMIT on a parenthesized query"));
            }
            select.order_by = order_by;
            select.limit = limit;
            select.offset = offset;
            Ok(ViewBody::Select(select))
        }
        ViewBody::Set {
            op,
            all,
            left,
            right,
            order_by: inner_order,
            limit: inner_limit,
            offset: inner_offset,
        } => {
            if !inner_order.is_empty() || inner_limit.is_some() || inner_offset.is_some() {
                return Err(not_yet(
                    "nested ORDER BY/LIMIT on a parenthesized set operation",
                ));
            }
            Ok(ViewBody::Set {
                op,
                all,
                left,
                right,
                order_by,
                limit,
                offset,
            })
        }
        // The outer tail of a parenthesized `WITH` query applies to its inner body's result — fold it
        // down onto that inner body (which itself rejects a doubled tail).
        ViewBody::With {
            recursive,
            ctes,
            body: inner,
        } => Ok(ViewBody::With {
            recursive,
            ctes,
            body: Box::new(apply_query_tail(*inner, query, dialect)?),
        }),
    }
}

/// Lowers one operand of a set operation into a [`ViewBody`]. `output_names` is the enclosing set's column
/// list, applied to each arm as a **fallback** (each arm keeps its own projection aliases; the list names
/// only an otherwise-unnamed projection — see [`lower_select`]), so it flows to every operand, not just the
/// leftmost. Handles the shapes the renderers emit and external SQL carries: a parenthesized operand
/// ([`SetExpr::Query`], which owns its `ORDER BY`/`LIMIT`), a bare `SELECT` ([`SetExpr::Select`] — no
/// per-operand `ORDER BY`/`LIMIT`), the SQLite `SELECT * FROM (<operand>)` wrapper (peeled back to the inner
/// operand), and an un-parenthesized nested set. Every arm lowers with the column list as a fallback
/// (`column_list_is_fallback = true`).
fn lower_set_operand(
    expr: &SetExpr,
    output_names: Option<&[String]>,
    dialect: SqlDialect,
) -> Result<ViewBody, ReadError> {
    match expr {
        // A parenthesized operand (a leaf `(SELECT …)` or a nested `(… UNION …)`): it is its own `Query`,
        // owning any per-operand `ORDER BY`/`LIMIT`.
        SetExpr::Query(inner) => lower_body(inner, output_names, true, dialect),
        SetExpr::Select(select) => {
            // Reject unsupported outer select-level clauses FIRST, so the SQLite-wrapper peel below cannot
            // bypass them: `SELECT * FROM (<operand>) QUALIFY …` matches the wrapper shape but carries a
            // `QUALIFY` (or `WINDOW`, `CONNECT BY`, …) that peeling would silently drop — it must surface
            // as `NotYetLowered` instead. (`lower_select` re-checks, harmlessly, on the non-wrapper path.)
            reject_unsupported_select_clauses(select)?;
            // SQLite wraps each operand as `SELECT * FROM (<operand>)`; peel it back to the inner operand.
            if let Some(inner) = sqlite_subquery_operand(select) {
                return lower_body(inner, output_names, true, dialect);
            }
            // A bare `SELECT` operand carries no per-operand `ORDER BY`/`LIMIT` (those live on a `Query`
            // wrapper), so the lowered select keeps its default empty tail.
            Ok(ViewBody::Select(Box::new(lower_select(
                select,
                output_names,
                true,
                dialect,
            )?)))
        }
        // An un-parenthesized nested set (`a UNION b INTERSECT c`): no per-operand `ORDER BY`/`LIMIT`.
        SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } => {
            let (set_op, all) = lower_set_op(op, set_quantifier)?;
            let left = lower_set_operand(left, output_names, dialect)?;
            let right = lower_set_operand(right, output_names, dialect)?;
            Ok(ViewBody::Set {
                op: set_op,
                all,
                left: Box::new(left),
                right: Box::new(right),
                order_by: Vec::new(),
                limit: None,
                offset: None,
            })
        }
        SetExpr::Values(_) => Err(not_yet("VALUES set operand")),
        other => Err(not_yet(format!("set operand `{other}`"))),
    }
}

/// Lowers a subquery in a scalar / `IN` / `EXISTS` expression position into a single-`SELECT`
/// [`ViewQueryModel`] — the shape those expression IR nodes hold. A set-operation subquery in that
/// position is a later phase, so it surfaces as [`ReadError::NotYetLowered`].
fn lower_subquery_model(query: &Query, dialect: SqlDialect) -> Result<ViewQueryModel, ReadError> {
    match lower_body(query, None, false, dialect)? {
        ViewBody::Select(select) => Ok(*select),
        ViewBody::Set { .. } => Err(not_yet("set operation in a scalar/IN/EXISTS subquery")),
        ViewBody::With { .. } => Err(not_yet("`WITH` (CTE) in a scalar/IN/EXISTS subquery")),
    }
}

/// Maps a parsed set operator + quantifier to the neutral [`ViewSetOp`] + `all` flag. `MINUS` and the
/// `BY NAME` quantifiers are non-standard / unmodeled → [`ReadError::NotYetLowered`].
fn lower_set_op(
    op: &SetOperator,
    quantifier: &SetQuantifier,
) -> Result<(ViewSetOp, bool), ReadError> {
    let set_op = match op {
        SetOperator::Union => ViewSetOp::Union,
        SetOperator::Intersect => ViewSetOp::Intersect,
        SetOperator::Except => ViewSetOp::Except,
        SetOperator::Minus => return Err(not_yet("MINUS set operator")),
    };
    let all = match quantifier {
        SetQuantifier::None | SetQuantifier::Distinct => false,
        SetQuantifier::All => true,
        SetQuantifier::ByName | SetQuantifier::AllByName | SetQuantifier::DistinctByName => {
            return Err(not_yet("set operation `BY NAME`"));
        }
    };
    Ok((set_op, all))
}

/// Recognizes the SQLite set-operand wrapper `SELECT * FROM (<subquery>)` — a bare `SELECT *` over a
/// single un-aliased derived table and nothing else — returning the inner subquery to lower in its place.
/// Any other clause (a `WHERE`, a join, `DISTINCT`, a named/aliased source, …) means it is a genuine
/// `SELECT *`, not the wrapper, so this returns `None` (and the genuine `SELECT *` stays unlowered).
fn sqlite_subquery_operand(select: &Select) -> Option<&Query> {
    // Exactly one projection, an unqualified `*` with no `EXCEPT`/`REPLACE`/… options.
    let [SelectItem::Wildcard(opts)] = select.projection.as_slice() else {
        return None;
    };
    if opts.opt_exclude.is_some()
        || opts.opt_except.is_some()
        || opts.opt_rename.is_some()
        || opts.opt_replace.is_some()
        || opts.opt_ilike.is_some()
        || opts.opt_alias.is_some()
    {
        return None;
    }
    // Exactly one FROM entry: an un-aliased, non-lateral derived table with no joins.
    let [table] = select.from.as_slice() else {
        return None;
    };
    if !table.joins.is_empty() {
        return None;
    }
    let TableFactor::Derived {
        lateral: false,
        subquery,
        alias: None,
        sample: None,
    } = &table.relation
    else {
        return None;
    };
    // No other clause may be present — otherwise it is a real `SELECT *`, not the wrapper.
    if select.distinct.is_some()
        || select.selection.is_some()
        || select.having.is_some()
        || !matches!(&select.group_by, GroupByExpr::Expressions(exprs, mods) if exprs.is_empty() && mods.is_empty())
    {
        return None;
    }
    Some(subquery)
}

fn not_yet(what: impl std::fmt::Display) -> ReadError {
    ReadError::NotYetLowered(what.to_string())
}

/// Rejects `CREATE VIEW` modifiers the renderer never emits and the body-only [`ViewQueryModel`] cannot
/// carry (`MATERIALIZED`, `TEMPORARY`, `SECURE`, `IF NOT EXISTS`, `WITH (…)` options, `CLUSTER BY`, a
/// view comment, a `TO`/params target, …). `OR REPLACE` is accepted — the renderer emits it and it does
/// not change the body (whether to re-create is a plan-time choice, not part of the model).
fn reject_unsupported_view_modifiers(create_view: &CreateView) -> Result<(), ReadError> {
    let CreateView {
        or_alter,
        materialized,
        secure,
        options,
        cluster_by,
        comment,
        with_no_schema_binding,
        if_not_exists,
        temporary,
        copy_grants,
        to,
        params,
        // Accepted / body-irrelevant: `or_replace` (emitted), `name`/`columns`/`query` (consumed),
        // `name_before_not_exists` (a spelling detail).
        or_replace: _,
        name: _,
        name_before_not_exists: _,
        columns: _,
        query: _,
    } = create_view;
    let unsupported = *or_alter
        || *materialized
        || *secure
        || *with_no_schema_binding
        || *if_not_exists
        || *temporary
        || *copy_grants
        || to.is_some()
        || params.is_some()
        || comment.is_some()
        || !cluster_by.is_empty()
        || !matches!(options, CreateTableOptions::None);
    if unsupported {
        return Err(not_yet(
            "CREATE VIEW modifier (MATERIALIZED / TEMPORARY / SECURE / IF NOT EXISTS / WITH options / \
             CLUSTER BY / comment / TO)",
        ));
    }
    Ok(())
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
        // EXCEPT PostgreSQL's `pg_get_constraintdef` synthesizes a `::type` cast on a LITERAL: a number to
        // a numeric type (`0` → `(0)::numeric`), a string to a text type (`'x'` → `('x')::text`), and — for
        // a *negative* number — a string cast to a numeric type (`-5` → `('-5')::integer`). Recover the
        // bare literal only when the cast is a guaranteed value-preserving no-op (so a published check
        // re-plans to empty); a *converting* cast (`'Infinity'::float8`, `(1.5)::integer`, `varchar(3)`, any
        // float target) is meaningful and left `NotYetLowered` (→ `Raw`, kept comparable by canonical.rs).
        //
        // A `::type` cast around a NON-literal operand is deliberately NOT stripped: it is ambiguous
        // without the operand's column type. PostgreSQL adds a value-preserving `::text` around an
        // already-text operand (`char_length((name)::text)` on a `varchar`), but the same syntactic shape
        // is a MEANINGFUL conversion on a non-text operand (`id::text LIKE '1%'`, `char_length(id::text)`
        // — digit count), and the two cannot be told apart here. So it stays `Raw` (kept comparable by
        // canonical.rs) rather than risk dropping a semantic cast. (A text-function check on an explicit
        // `varchar` column may therefore churn — a documented limitation, never corruption.)
        Expr::Cast {
            kind: CastKind::DoubleColon,
            expr,
            data_type,
            ..
        } if dialect == SqlDialect::Postgres => {
            let inner = strip_nested(expr);
            // A `::type` on a LITERAL is the redundant deparse cast recovered above (`(0)::numeric`).
            // Otherwise a `::type` around a pinnable aggregate / window / `EXTRACT` is a result-pin in
            // PostgreSQL's `pg_get_viewdef` deparse — it emits the `::` form `(sum(x))::bigint` where the
            // renderer writes the function-style `CAST(sum(x) AS bigint)`. Peel it the same way (a `::` on
            // any other non-literal, e.g. a bare column, stays `NotYetLowered` via `lower_result_pin`).
            match redundant_cast_literal(inner, data_type) {
                Some(literal) => Ok(literal),
                None => lower_result_pin(inner, data_type, dialect),
            }
        }

        // A function-style `CAST(<call> AS ty)` is the renderer's result-pin: an OUTER cast wrapping an
        // aggregate / window / `EXTRACT` so the output column's wire type is uniform across dialects.
        // Peel the cast into the wrapped node's `result` field. A cast around anything else is a general
        // user cast (dialect-ambiguous target spelling), still `NotYetLowered`.
        Expr::Cast {
            kind: CastKind::Cast,
            expr,
            data_type,
            format: None,
            array: false,
        } => lower_result_pin(expr, data_type, dialect),

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

        // `EXTRACT(<field> FROM <operand>)` with no outer result-pin (`result: None`). `SECOND` is the
        // fractional-seconds node; every other field is the integer `Extract`.
        Expr::Extract {
            field,
            expr,
            syntax: ExtractSyntax::From,
        } => lower_extract(field, expr, None, dialect),

        // A bare `FLOOR(EXTRACT(SECOND FROM x))` (no result-pin) is the whole-seconds `Extract` for the
        // `Second` field (the renderer floors PostgreSQL's fractional `EXTRACT(SECOND …)`).
        Expr::Floor {
            expr,
            field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime),
        } => lower_floored_second(expr, None, dialect),

        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => lower_case(
            operand.as_deref(),
            conditions,
            else_result.as_deref(),
            dialect,
        ),

        // Subqueries in a scalar / `IN` / `EXISTS` position lower to a single-`SELECT` `ViewQueryModel`
        // (the expression-subquery IR node is select-only; a set-op subquery there is a later phase).
        Expr::Subquery(query) => Ok(ExprNode::ScalarSubquery(Box::new(lower_subquery_model(
            query, dialect,
        )?))),
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Ok(ExprNode::InSubquery {
            negated: *negated,
            operand: b(lower(expr, dialect)?),
            subquery: Box::new(lower_subquery_model(subquery, dialect)?),
        }),
        Expr::Exists { subquery, negated } => Ok(ExprNode::Exists {
            negated: *negated,
            subquery: Box::new(lower_subquery_model(subquery, dialect)?),
        }),

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
        // `%` renders bare on every dialect and round-trips structurally on the same backend. Unlike
        // `/` (whose bare/float-cast forms carry different neutral semantics, handled below), a bare `%`
        // needs no dialect gating here: it always maps to the neutral `Modulo`. The one cross-dialect
        // caveat — SQLite integer-coerces `%` operands while PG/MySQL keep the remainder — is a semantic
        // portability limitation documented on `ArithmeticOp::Modulo`, not a lowering ambiguity.
        BinaryOperator::Modulo => Some(ArithmeticOp::Modulo),
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
        // `lower_function`). Only fold to `Concat` on the dialects where `||` is concatenation. The
        // renderer joins all args of one `Concat` node with `||` inside a single paren pair, so an N-arg
        // `Concat` renders flat `(a || b || … )` while a nested `Concat` wraps each sub-node in its own
        // parens (`((a || b) || c)`); flatten a bare `||` chain into one flat `Concat` (matching the flat
        // render) but keep a parenthesized operand as a single nested arg, so both shapes re-render
        // byte-identically.
        BinaryOperator::StringConcat if pipe_is_concatenation(dialect) => {
            let mut args = Vec::new();
            flatten_pipe_concat(left, dialect, &mut args)?;
            args.push(lower(right, dialect)?);
            Ok(ExprNode::ScalarFn {
                func: ScalarFunc::Concat,
                args,
            })
        }
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

/// Flattens a bare `||` concat chain into `args`, matching the renderer's flat output for one `Concat`
/// node. A parenthesized (`Nested`) operand is NOT descended into — it is an explicitly nested
/// sub-`Concat` that stays a single argument (`((a || b) || c)`) — so the flat and nested render shapes
/// stay distinguishable and each re-renders byte-identically. Only called for a `||`-concatenation
/// dialect (the caller is gated on `pipe_is_concatenation`).
fn flatten_pipe_concat(
    expr: &Expr,
    dialect: SqlDialect,
    args: &mut Vec<ExprNode>,
) -> Result<(), ReadError> {
    // A *bare* left-nested `||` chain (its left operand is itself a `||`, not wrapped in parens); descend
    // so `a || b || c` becomes the flat `[a, b, c]`.
    if let Expr::BinaryOp {
        left,
        op: BinaryOperator::StringConcat,
        right,
    } = expr
    {
        flatten_pipe_concat(left, dialect, args)?;
        args.push(lower(right, dialect)?);
        Ok(())
    } else {
        args.push(lower(expr, dialect)?);
        Ok(())
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

/// Recovers the bare [`ExprNode::Literal`] from a PostgreSQL deparse cast on a literal, but only when the
/// cast is a guaranteed value-preserving no-op; otherwise `None` (the caller reports `NotYetLowered`).
///
/// Handles the three forms `pg_get_constraintdef` produces: a number cast to a numeric type
/// (`(0)::numeric`), a string cast to a text type (`('x')::text`), and — how PostgreSQL renders a
/// *negative* number — a string cast to a numeric type (`('-5')::integer`, `('-1.5')::numeric`). A cast
/// that converts (fractional→integer, any float target, bounded `varchar(n)`/`numeric(p,s)`, string→date)
/// is not a no-op and yields `None`.
fn redundant_cast_literal(expr: &Expr, data_type: &DataType) -> Option<ExprNode> {
    let Expr::Value(value) = expr else {
        return None;
    };
    match &value.value {
        Value::Number(text, _) if numeric_cast_is_noop(text, data_type) => {
            Some(ExprNode::Literal(text.clone()))
        }
        Value::SingleQuotedString(text) => {
            // A string cast to an UNBOUNDED text type is a no-op regardless of the string's content —
            // including a numeric-looking string like `('0')::text` from a text check `code <> '0'`. Check
            // the text target FIRST so such a string stays a quoted string literal. (A bounded
            // `varchar(n)`/`char(n)` can truncate/pad, and a non-text target converts → not a no-op.)
            if is_unbounded_text_type(data_type) {
                Some(ExprNode::Literal(format!("'{}'", text.replace('\'', "''"))))
            } else if is_numeric_literal(text) && numeric_cast_is_noop(text, data_type) {
                // PostgreSQL's negative-number deparse: `('-5')::integer`. Recover the bare number.
                Some(ExprNode::Literal(text.clone()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether the decimal literal `text` casts to `data_type` without changing value: an integer literal is
/// exact in any numeric type; a fractional literal only in a non-integer numeric type (an integer type
/// truncates it).
fn numeric_cast_is_noop(text: &str, data_type: &DataType) -> bool {
    if text.contains(['.', 'e', 'E']) {
        is_numeric_type(data_type) && !is_integer_type(data_type)
    } else {
        is_numeric_type(data_type)
    }
}

/// Whether `text` is a plain decimal numeric literal (optional leading sign): the content PostgreSQL puts
/// in a `('-5')::integer`-style negative-literal cast.
fn is_numeric_literal(text: &str) -> bool {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    !digits.is_empty()
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && digits.bytes().filter(|byte| *byte == b'.').count() <= 1
}

/// Whether `data_type` is an UNBOUNDED text type (`text`, or `varchar`/`character varying` with no
/// length): a cast to it neither truncates nor pads, so it is a value-preserving no-op.
fn is_unbounded_text_type(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Text
            | DataType::Varchar(None)
            | DataType::CharVarying(None)
            | DataType::CharacterVarying(None)
    )
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
/// re-rendered with the wrong precision. Both the renderer's function-style `CAST(inner AS ty)` and
/// PostgreSQL's `pg_get_viewdef` `::` deparse (`(inner)::double precision`) are accepted (`::` parses only
/// on PostgreSQL/Generic — SQLite stores the verbatim function-style form — so there is no ambiguity).
fn float_cast_operand(expr: &Expr, dialect: SqlDialect) -> Option<&Expr> {
    let idiom_type = match dialect {
        SqlDialect::Postgres => DataType::DoublePrecision,
        SqlDialect::Sqlite => DataType::Real,
        // MySQL renders a neutral `Divide` bare (no cast); `Generic` is not a round-trip target.
        SqlDialect::Mysql | SqlDialect::Generic => return None,
    };
    match expr {
        Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            format: None,
            array: false,
        } if *data_type == idiom_type => Some(expr),
        _ => None,
    }
}

/// Dispatches a parsed function call. A windowed call (`OVER`) becomes an [`ExprNode::Window`]; a
/// single, unquoted aggregate / `CURRENT_TIMESTAMP` / `COALESCE` / `NULLIF` name becomes its dedicated
/// view-body node; everything else is a scalar / general function ([`lower_scalar_function`]).
fn lower_function(function: &Function, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    // A windowed call `FUNC(args) OVER (…)` — handled before the scalar guards, which reject `OVER`.
    if let Some(over) = &function.over {
        return lower_window(function, over, None, dialect);
    }
    // The view-body call forms are keyed by a single, *unquoted* function name (a quoted name is a
    // user identifier whose case must not be folded — it falls through to the general path).
    if let Some(name) = single_unquoted_name(function) {
        if let Some(func) = aggregate_func(&name) {
            return lower_aggregate(function, func, None, dialect);
        }
        match name.as_str() {
            "current_timestamp" => return lower_now(function, dialect),
            "coalesce" => return lower_coalesce(function, dialect),
            "nullif" => return lower_nullif(function, dialect),
            "date_trunc" => return lower_date_trunc(function, dialect),
            _ => {}
        }
    }
    lower_scalar_function(function, dialect)
}

/// The lowercased name of a call with a single, *unquoted* identifier name (`SUM`, `coalesce`); `None`
/// for a qualified (`schema.f`) or quoted (`"MyFunc"`) name, which never denotes a built-in view-body
/// call form.
fn single_unquoted_name(function: &Function) -> Option<String> {
    let ident = match function.name.0.as_slice() {
        [part] => part.as_ident()?,
        _ => return None,
    };
    if ident.quote_style.is_some() {
        return None;
    }
    Some(ident.value.to_ascii_lowercase())
}

/// Maps an aggregate function name (as the renderer's `aggregate_name` spells it, case-folded) to its
/// [`AggregateFunc`]; `None` for a non-aggregate name.
fn aggregate_func(name: &str) -> Option<AggregateFunc> {
    match name {
        "count" => Some(AggregateFunc::Count),
        "sum" => Some(AggregateFunc::Sum),
        "avg" => Some(AggregateFunc::Avg),
        "min" => Some(AggregateFunc::Min),
        "max" => Some(AggregateFunc::Max),
        _ => None,
    }
}

fn lower_scalar_function(function: &Function, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
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
    let name_ident = match function.name.0.as_slice() {
        [part] => part.as_ident(),
        _ => None,
    }
    .ok_or_else(|| not_yet(format!("qualified function name `{}`", function.name)))?;
    // The name is folded to lowercase to match PostgreSQL's unquoted deparse; whether it was quoted in
    // the source gates the general fallback below (a quoted mixed-case name must not be folded lossily).
    let name = name_ident.value.to_ascii_lowercase();
    let name_is_quoted = name_ident.quote_style.is_some();

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
        // Any other *unquoted* function with *no direct literal argument* is a general, dialect-neutral
        // call: the closed `ScalarFn` set only covers the functions whose rendering diverges across
        // dialects (`CHAR_LENGTH`/`LENGTH`, `||`/`CONCAT`, `substr`/`SUBSTRING`); every other function —
        // user-defined or built-in like `jsonb_typeof` — renders its name verbatim, so it lowers to a
        // general `Function` node. The name is folded to lowercase (faithful to PostgreSQL's deparse of an
        // unquoted name). A published `jsonb_typeof(data) = 'object'` check re-plans to empty: the function
        // takes a column argument (no synthesized cast), and the literal `'object'::text` cast pg adds to
        // the *comparison* operand is stripped where operand casts always are.
        //
        // Two shapes are deliberately kept `Raw` instead (normalized as a string by the backend's
        // `canonical_check_expression`, or held verbatim by the generated/index seams):
        //  - a *quoted* function name (`"MyFunc"`) — folding its case would change which overload the call
        //    resolves to;
        //  - a *direct literal argument* (`my_func('x')`) — pg deparses it as `my_func('x'::text)`, and
        //    stripping that synthesized arg cast to converge would rewrite the term the canonical model
        //    feeds into `GENERATED`/`CREATE INDEX` DDL, potentially resolving a different overload.
        _ if !name_is_quoted && !args.iter().any(|arg| matches!(arg, ExprNode::Literal(_))) => {
            Ok(ExprNode::Function { name, args })
        }
        _ => Err(not_yet(format!(
            "general function `{}` (quoted name or literal argument)",
            function.name
        ))),
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

// ===== view-body node lowering (aggregate / window / CASE / EXTRACT / subquery) =====

/// Peels the renderer's result-pin — an OUTER `CAST(<call> AS ty)` around an aggregate / window /
/// `EXTRACT` — into the wrapped node's `result` field. The target `ty` is inverted from the parsed
/// [`DataType`] via this dialect's cast vocabulary; a type the vocabulary cannot map to an exact
/// [`SqlType`] yields `NotYetLowered` (guessing a different type would churn the re-render). A cast
/// around anything that is *not* a pinnable call is a general user cast, also `NotYetLowered`.
fn lower_result_pin(
    inner: &Expr,
    data_type: &DataType,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    let ty = invert_pin_type(data_type, dialect)
        .ok_or_else(|| not_yet(format!("cast to `{data_type}`")))?;
    match inner {
        // A windowed call keeps its `OVER (…)` — pin the window's result.
        Expr::Function(function) if function.over.is_some() => {
            let over = function.over.as_ref().expect("checked is_some");
            lower_window(function, over, Some(ty), dialect)
        }
        // Only an aggregate call is otherwise pinned (a plain scalar/general function self-types).
        Expr::Function(function) => {
            let func = single_unquoted_name(function)
                .as_deref()
                .and_then(aggregate_func)
                .ok_or_else(|| not_yet(format!("cast around function `{function}`")))?;
            lower_aggregate(function, func, Some(ty), dialect)
        }
        Expr::Extract {
            field,
            expr,
            syntax: ExtractSyntax::From,
        } => lower_extract(field, expr, Some(ty), dialect),
        Expr::Floor {
            expr,
            field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime),
        } => lower_floored_second(expr, Some(ty), dialect),
        other => Err(not_yet(format!("cast around `{other}`"))),
    }
}

/// Inverts a parsed cast-target [`DataType`] back to the neutral [`SqlType`] this dialect's renderer
/// would have emitted it from. Dialect-specific because each dialect spells cast types differently
/// (`bigint` vs `SIGNED` vs `INTEGER`). Returns `None` for a spelling this inverter does not recognize,
/// so the caller reports `NotYetLowered` rather than guess.
///
/// PostgreSQL's cast spellings are one-to-one for the common widths, so the inverse is *exact*. MySQL's
/// cast vocabulary is lossy — every integer width collapses to `SIGNED` — so the inverse is a canonical
/// representative (`SIGNED` → [`SqlType::I64`]) that re-renders to the same keyword (preserving the
/// round-trip identity invariant) but may not equal the original narrower type structurally. SQLite's
/// affinity names are likewise many-to-one; its canonical inverse re-renders identically.
fn invert_pin_type(data_type: &DataType, dialect: SqlDialect) -> Option<SqlType> {
    match dialect {
        SqlDialect::Postgres => invert_pg_cast_type(data_type),
        SqlDialect::Mysql => invert_mysql_cast_type(data_type),
        SqlDialect::Sqlite => invert_sqlite_cast_type(data_type),
        // `Generic` is not a render target, so it emits no result-pin idiom to invert.
        SqlDialect::Generic => None,
    }
}

/// Inverse of PostgreSQL's `write_pg_sql_type` over the whole cast vocabulary a result-pin can carry (the
/// pin's type is the view's output column type — any [`SqlType`]). Mostly exact — each PostgreSQL keyword
/// maps back to the `SqlType` it is rendered from — with two documented many-to-one collapses that take a
/// canonical representative (re-rendering to the same keyword, so round-trip identity holds, though the
/// residual narrower type is the backend PR's `canonical_view_*` job): `smallint`←`I8`/`I16`/`U8`,
/// `integer`←`I32`/`U16`, `bigint`←`I64`/`Isize`/`U32`/`Usize`, bare `numeric`←`I128`/`U64`/`U128`. A
/// `numeric(p, s)` is a `Decimal` and inverts exactly.
fn invert_pg_cast_type(data_type: &DataType) -> Option<SqlType> {
    use sqlparser::ast::ExactNumberInfo::{None as NoInfo, PrecisionAndScale};
    let ty = match data_type {
        DataType::Bool | DataType::Boolean => SqlType::Bool,
        DataType::SmallInt(None) | DataType::Int2(None) => SqlType::I16,
        DataType::Integer(None) | DataType::Int(None) | DataType::Int4(None) => SqlType::I32,
        DataType::BigInt(None) | DataType::Int8(None) => SqlType::I64,
        DataType::Real | DataType::Float4 => SqlType::F32,
        DataType::DoublePrecision | DataType::Float8 => SqlType::F64,
        // PostgreSQL renders both `SqlType::String` and `SqlType::Text` as `text`, and introspection
        // canonicalizes `text` back to `String` (introspect.rs). Invert to the same `String` so a
        // `String`-pinned view compares equal to its introspected form (both render `text`).
        DataType::Text => SqlType::String,
        DataType::Uuid => SqlType::Uuid,
        DataType::JSON => SqlType::Json,
        DataType::JSONB => SqlType::Jsonb,
        DataType::Bytea => SqlType::Bytes,
        DataType::Date => SqlType::Date,
        // Bare `numeric` is the pin for a 128-bit / wide-unsigned integer (all render `numeric`); canonical
        // `I128`. A precision/scale `numeric(p, s)` is a `Decimal` and inverts exactly.
        DataType::Numeric(NoInfo) | DataType::Decimal(NoInfo) | DataType::Dec(NoInfo) => {
            SqlType::I128
        }
        DataType::Numeric(PrecisionAndScale(p, s))
        | DataType::Decimal(PrecisionAndScale(p, s))
        | DataType::Dec(PrecisionAndScale(p, s)) => SqlType::Decimal {
            precision: *p as u32,
            scale: *s as u32,
        },
        // The renderer emits `varchar(n)`, but PostgreSQL's `pg_get_viewdef` deparses the same cast as
        // `character varying(n)` — accept both spellings for a `Varchar` pin.
        DataType::Varchar(Some(length))
        | DataType::CharVarying(Some(length))
        | DataType::CharacterVarying(Some(length)) => SqlType::Varchar(character_length(length)?),
        DataType::Char(Some(length)) | DataType::Character(Some(length)) => {
            SqlType::Char(character_length(length)?)
        }
        DataType::Time(precision, tz) => SqlType::Time {
            tz: is_with_time_zone(tz),
            precision: fsp(*precision),
        },
        DataType::Timestamp(precision, tz) => SqlType::Timestamp {
            tz: is_with_time_zone(tz),
            precision: fsp(*precision),
        },
        _ => return None,
    };
    Some(ty)
}

/// Inverse of MySQL's `write_cast_type` for the result-pin cast vocabulary. Lossy: MySQL's cast keywords
/// are many-to-one (`SIGNED` for every signed-integer width, `CHAR` for every text-like type, `BINARY`
/// for both binary widths, `DATETIME` drops a timestamp's time zone, `DECIMAL(65, 0)` for both 128-bit
/// ints), so this returns a canonical representative that re-renders to the same keyword (round-trip
/// identity preserved) but is not guaranteed to equal a narrower/tz-carrying original structurally.
/// Reconciling that residual difference so a MySQL view re-plans to empty is the MySQL backend PR's
/// `canonical_view_*` seam. A *bare* `DECIMAL` (a `Decimal` pin, whose precision/scale the keyword drops)
/// is not inverted — its precision cannot be recovered.
fn invert_mysql_cast_type(data_type: &DataType) -> Option<SqlType> {
    use sqlparser::ast::ExactNumberInfo::PrecisionAndScale;
    let ty = match data_type {
        DataType::Signed | DataType::SignedInteger => SqlType::I64,
        DataType::Unsigned | DataType::UnsignedInteger => SqlType::U64,
        DataType::Double(_) => SqlType::F64,
        // `CHAR` is MySQL's cast keyword for every text-like type (`Text`/`Uuid`/`Json`/…); canonical `Text`.
        DataType::Char(None) => SqlType::Text,
        // `BINARY` covers both variable- and fixed-width binary; canonical `Bytes`.
        DataType::Binary(None) => SqlType::Bytes,
        DataType::Date => SqlType::Date,
        // `DATETIME(n)`/`TIME(n)` are tz-naive casts for a `Timestamp`/`Time` pin — the canonical inverse
        // drops the time zone (`tz: false`).
        DataType::Datetime(precision) => SqlType::Timestamp {
            tz: false,
            precision: fsp(*precision),
        },
        DataType::Time(precision, _) => SqlType::Time {
            tz: false,
            precision: fsp(*precision),
        },
        // `DECIMAL(65, 0)` is the widened cast for a 128-bit int (both `I128`/`U128`); canonical `I128`.
        DataType::Decimal(PrecisionAndScale(65, 0)) => SqlType::I128,
        // Any other `DECIMAL` is a `Decimal` pin. MySQL's cast renders bare `DECIMAL` (dropping
        // precision/scale), so a canonical `Decimal` re-renders to the same keyword (identity holds; the
        // exact precision is the backend PR's `canonical_view_*` job).
        DataType::Decimal(info) => decimal_from_exact(info),
        _ => return None,
    };
    Some(ty)
}

/// Narrows a parsed fractional-seconds precision to the model's width (fsp is 0..=6).
fn fsp(precision: Option<u64>) -> Option<u8> {
    precision.map(|p| p as u8)
}

/// A canonical [`SqlType::Decimal`] from a parsed `DECIMAL`/`NUMERIC` precision spec. A bare form (no
/// precision) takes the `(10, 0)` default MySQL/SQLite apply; those dialects drop the precision on render
/// (a bare `DECIMAL` / a `NUMERIC` affinity), so the canonical re-renders to the same spelling regardless.
fn decimal_from_exact(info: &sqlparser::ast::ExactNumberInfo) -> SqlType {
    use sqlparser::ast::ExactNumberInfo;
    let (precision, scale) = match info {
        ExactNumberInfo::None => (10, 0),
        ExactNumberInfo::Precision(p) => (*p as u32, 0),
        ExactNumberInfo::PrecisionAndScale(p, s) => (*p as u32, *s as u32),
    };
    SqlType::Decimal { precision, scale }
}

/// Whether a parsed temporal type carries the `with time zone` suffix (PostgreSQL `timestamptz`).
fn is_with_time_zone(tz: &sqlparser::ast::TimezoneInfo) -> bool {
    matches!(
        tz,
        sqlparser::ast::TimezoneInfo::WithTimeZone | sqlparser::ast::TimezoneInfo::Tz
    )
}

/// The integer length of a `varchar(n)`/`char(n)` cast target (in the default character unit); `None` for
/// a `MAX` or unit-qualified length, which squealy never renders.
fn character_length(length: &sqlparser::ast::CharacterLength) -> Option<u32> {
    match length {
        sqlparser::ast::CharacterLength::IntegerLength { length, unit: None } => {
            Some(*length as u32)
        }
        _ => None,
    }
}

/// Inverse of SQLite's `sqlite_affinity` for the result-pin cast vocabulary. Deeply lossy — SQLite has
/// five affinities, so every integer width is `INTEGER`, every text-like type `TEXT`, both binary widths
/// `BLOB`, and a `Decimal` is `NUMERIC` — and this returns the canonical representative for each, which
/// re-renders to the same affinity name (the exact original type is the backend PR's `canonical_view_*`
/// job; SQLite compares view columns by name regardless).
fn invert_sqlite_cast_type(data_type: &DataType) -> Option<SqlType> {
    match data_type {
        DataType::Integer(None) | DataType::Int(None) => Some(SqlType::I64),
        DataType::Real => Some(SqlType::F64),
        DataType::Text => Some(SqlType::Text),
        DataType::Blob(None) => Some(SqlType::Bytes),
        // A `NUMERIC` affinity comes from a `Decimal` pin; the affinity drops the precision, so a canonical
        // `Decimal` re-renders to the same `NUMERIC`.
        DataType::Numeric(info) => Some(decimal_from_exact(info)),
        _ => None,
    }
}

/// Lowers an aggregate call `FUNC([DISTINCT] <operand>)` into an [`ExprNode::Aggregate`]. `result` is
/// `Some` when peeled from an outer result-pin cast, else `None` (the un-pinned `COUNT(id)` form).
fn lower_aggregate(
    function: &Function,
    func: AggregateFunc,
    result: Option<SqlType>,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    // The renderer emits a bare `FUNC([DISTINCT] x)` — no `FILTER`, `WITHIN GROUP`, ordering clause, or
    // `IGNORE NULLS`.
    if function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.parameters != FunctionArguments::None
    {
        return Err(not_yet(format!("aggregate call `{function}`")));
    }
    let (distinct, operand) = match &function.args {
        FunctionArguments::List(list) if list.clauses.is_empty() => {
            let distinct = match list.duplicate_treatment {
                None => false,
                Some(DuplicateTreatment::Distinct) => true,
                // The renderer never emits an explicit `ALL`.
                Some(DuplicateTreatment::All) => {
                    return Err(not_yet("aggregate with explicit `ALL`"));
                }
            };
            match list.args.as_slice() {
                [FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))] => {
                    (distinct, lower(expr, dialect)?)
                }
                // `COUNT(*)` (wildcard) and multi-argument aggregates are outside the emitted grammar.
                _ => return Err(not_yet(format!("aggregate arguments of `{function}`"))),
            }
        }
        _ => return Err(not_yet(format!("aggregate arguments of `{function}`"))),
    };
    Ok(ExprNode::Aggregate {
        func,
        distinct,
        operand: b(operand),
        result,
    })
}

/// Lowers a windowed call `FUNC(<args>) OVER (PARTITION BY … ORDER BY …)` into an [`ExprNode::Window`].
/// A window *frame* is not yet inverted (returns `NotYetLowered`); the renderer's simple windows carry
/// none.
fn lower_window(
    function: &Function,
    over: &WindowType,
    result: Option<SqlType>,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    let spec = match over {
        WindowType::WindowSpec(spec) => spec,
        WindowType::NamedWindow(_) => return Err(not_yet("named window reference")),
    };
    if function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.parameters != FunctionArguments::None
        || spec.window_name.is_some()
    {
        return Err(not_yet(format!("window call `{function}`")));
    }
    // A frame (`ROWS`/`RANGE BETWEEN …`) is not yet inverted.
    if spec.window_frame.is_some() {
        return Err(not_yet("window frame clause"));
    }
    let func = single_unquoted_name(function)
        .as_deref()
        .and_then(window_func)
        .ok_or_else(|| not_yet(format!("window function name of `{function}`")))?;
    let args = match &function.args {
        FunctionArguments::None => Vec::new(),
        FunctionArguments::List(list)
            if list.duplicate_treatment.is_none() && list.clauses.is_empty() =>
        {
            list.args
                .iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => lower(expr, dialect),
                    other => Err(not_yet(format!("window argument `{other}`"))),
                })
                .collect::<Result<_, _>>()?
        }
        _ => return Err(not_yet(format!("window arguments of `{function}`"))),
    };
    let partition_by = spec
        .partition_by
        .iter()
        .map(|expr| lower(expr, dialect))
        .collect::<Result<_, _>>()?;
    let order_by = spec
        .order_by
        .iter()
        .map(|order| lower_window_order(order, dialect))
        .collect::<Result<_, _>>()?;
    Ok(ExprNode::Window {
        func,
        args,
        partition_by,
        order_by,
        frame: None,
        result,
    })
}

/// Maps a window function name (as `window_func_name` spells it, case-folded) to its [`WindowFunc`],
/// including an aggregate used as a window (`SUM(x) OVER (…)`).
fn window_func(name: &str) -> Option<WindowFunc> {
    match name {
        "row_number" => Some(WindowFunc::RowNumber),
        "rank" => Some(WindowFunc::Rank),
        "dense_rank" => Some(WindowFunc::DenseRank),
        "ntile" => Some(WindowFunc::Ntile),
        "lag" => Some(WindowFunc::Lag),
        "lead" => Some(WindowFunc::Lead),
        other => aggregate_func(other).map(WindowFunc::Aggregate),
    }
}

/// Lowers one `ORDER BY` term inside a window `OVER (…)`. The renderer always writes an explicit
/// `ASC`/`DESC` and no `NULLS`; an omitted direction or a `NULLS` modifier is outside that grammar.
fn lower_window_order(
    order: &OrderByExpr,
    dialect: SqlDialect,
) -> Result<WindowOrderTerm, ReadError> {
    if order.with_fill.is_some() || order.options.nulls_first.is_some() {
        return Err(not_yet("window ORDER BY with WITH FILL / NULLS modifier"));
    }
    let direction = match order.options.asc {
        Some(true) => OrderDirection::Asc,
        Some(false) => OrderDirection::Desc,
        None => return Err(not_yet("window ORDER BY without explicit ASC/DESC")),
    };
    Ok(WindowOrderTerm {
        expr: lower(&order.expr, dialect)?,
        direction,
    })
}

/// Lowers `CURRENT_TIMESTAMP` / `CURRENT_TIMESTAMP(<digits>)` into [`ExprNode::Now`]. `Now` carries no
/// precision — it is re-derived per dialect on render — so only the *exact* spelling this dialect emits is
/// accepted: MySQL renders `CURRENT_TIMESTAMP(6)` (its `now_fractional_digits`), every other dialect the
/// bare `CURRENT_TIMESTAMP`. A different precision (`CURRENT_TIMESTAMP(3)`, or a bare call read as MySQL)
/// would re-render as this dialect's form and silently change the fractional-seconds precision, so it is
/// left `NotYetLowered` rather than lowered lossily.
fn lower_now(function: &Function, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    let digits = current_timestamp_digits(function)?;
    let expected = match dialect {
        SqlDialect::Mysql => Some(6),
        // PostgreSQL/SQLite render `now()` as the bare keyword; `Generic` authoring is bare too.
        SqlDialect::Postgres | SqlDialect::Sqlite | SqlDialect::Generic => None,
    };
    if digits == expected {
        Ok(ExprNode::Now)
    } else {
        Err(not_yet(format!(
            "CURRENT_TIMESTAMP precision `{function}` (this dialect's now() renders {})",
            match expected {
                Some(d) => format!("CURRENT_TIMESTAMP({d})"),
                None => "a bare CURRENT_TIMESTAMP".to_owned(),
            }
        )))
    }
}

/// The parsed precision of a `CURRENT_TIMESTAMP[(n)]` call: `None` for the bare form, `Some(n)` for an
/// explicit integer precision. A non-integer, multi-argument, or otherwise-decorated call is outside the
/// grammar (`NotYetLowered`).
fn current_timestamp_digits(function: &Function) -> Result<Option<u64>, ReadError> {
    let args = match &function.args {
        FunctionArguments::None => return Ok(None),
        FunctionArguments::List(list)
            if list.duplicate_treatment.is_none() && list.clauses.is_empty() =>
        {
            list.args.as_slice()
        }
        _ => return Err(not_yet(format!("CURRENT_TIMESTAMP call `{function}`"))),
    };
    match args {
        [] => Ok(None),
        [FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value)))] => match &value.value {
            Value::Number(number, false) => number
                .parse::<u64>()
                .map(Some)
                .map_err(|_| not_yet(format!("CURRENT_TIMESTAMP precision `{number}`"))),
            _ => Err(not_yet(format!("CURRENT_TIMESTAMP call `{function}`"))),
        },
        _ => Err(not_yet(format!("CURRENT_TIMESTAMP call `{function}`"))),
    }
}

/// Lowers `COALESCE(<args>)`, recovering the per-argument literal cast into `result` (present only when
/// every argument is an inlined literal; see [`recover_branch_casts`]).
fn lower_coalesce(function: &Function, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    let values = unnamed_args(function)?;
    let (args, result) = recover_branch_casts(&values, dialect, true)?;
    Ok(ExprNode::Coalesce { args, result })
}

/// Lowers `NULLIF(<left>, <right>)`, recovering the per-operand literal cast into `result` (present only
/// when both operands are inlined literals).
fn lower_nullif(function: &Function, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    let values = unnamed_args(function)?;
    let [left, right] = values.as_slice() else {
        return Err(not_yet(format!("NULLIF call `{function}`")));
    };
    let (mut args, result) = recover_branch_casts(&[left, right], dialect, true)?;
    let right = args.pop().expect("two operands");
    let left = args.pop().expect("two operands");
    Ok(ExprNode::Nullif {
        left: b(left),
        right: b(right),
        result,
    })
}

/// The plain, unnamed argument expressions of a call — no `DISTINCT`, ordering clauses, or named
/// arguments (the forms the `COALESCE`/`NULLIF` renderer never emits).
fn unnamed_args(function: &Function) -> Result<Vec<&Expr>, ReadError> {
    match &function.args {
        FunctionArguments::List(list)
            if list.duplicate_treatment.is_none() && list.clauses.is_empty() =>
        {
            list.args
                .iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Ok(expr),
                    other => Err(not_yet(format!("function argument `{other}`"))),
                })
                .collect()
        }
        _ => Err(not_yet(format!("function arguments of `{function}`"))),
    }
}

/// Lowers a `CASE` expression — searched ([`ExprNode::Case`], `operand: None`) or simple
/// ([`ExprNode::SimpleCase`]). The result-pin cast wraps each `THEN`/`ELSE` value (never the `WHEN`
/// conditions or a simple `CASE`'s operand); [`recover_branch_casts`] peels it back into `result`.
fn lower_case(
    operand: Option<&Expr>,
    conditions: &[CaseWhen],
    else_result: Option<&Expr>,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    // The branch VALUES (each `THEN`, then the `ELSE`) carry the per-branch cast; recover it uniformly.
    let mut values: Vec<&Expr> = conditions.iter().map(|when| &when.result).collect();
    if let Some(else_value) = else_result {
        values.push(else_value);
    }
    let (lowered_values, result) = recover_branch_casts(&values, dialect, false)?;
    let mut lowered = lowered_values.into_iter();
    let arms = conditions
        .iter()
        .map(|when| {
            Ok::<_, ReadError>(CaseArm {
                when: b(lower(&when.condition, dialect)?),
                then: b(lowered.next().expect("one value per WHEN arm")),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let else_ = else_result.map(|_| b(lowered.next().expect("ELSE value")));
    match operand {
        None => Ok(ExprNode::Case {
            arms,
            else_,
            result,
        }),
        Some(operand) => Ok(ExprNode::SimpleCase {
            operand: b(lower(operand, dialect)?),
            arms,
            else_,
            result,
        }),
    }
}

/// Recovers the per-branch result-pin cast shared by `CASE`/`COALESCE`/`NULLIF` branch values.
///
/// The renderer wraps each branch value in `CAST(<value> AS <result>)` — for `CASE` whenever `result`
/// is set, and for `COALESCE`/`NULLIF` only when *every* operand is an inlined literal (`literal_only`) —
/// so a genuine result-pin casts *every* branch, uniformly, to one type. Two cases:
///
/// - **Every branch is a cast to one consistent type** → the pin: the values are the bare inners and
///   `result` is `Some(ty)`.
/// - **Otherwise** → `result` is `None` and the values lower as-is. PostgreSQL's `pg_get_viewdef` still
///   wraps a bare *literal* branch in a redundant `::type` cast (`(0)::bigint`) even in an un-pinned
///   expression, so a `::` literal cast on *some* (not all) branches is deparse noise, not a mixed pin —
///   `lower` strips it to the bare literal. A *function-style* cast, or a non-redundant `::` cast, on some
///   but not all branches is a genuine mix the renderer never emits → `NotYetLowered`.
fn recover_branch_casts(
    values: &[&Expr],
    dialect: SqlDialect,
    literal_only: bool,
) -> Result<(Vec<ExprNode>, Option<SqlType>), ReadError> {
    if values.is_empty() {
        return Ok((Vec::new(), None));
    }
    // A per-branch cast the renderer could have emitted: `CAST(v AS ty)` (or pg's `(v)::ty` deparse) with
    // an invertible target and — for `COALESCE`/`NULLIF` — a literal operand.
    let casts: Vec<Option<(&Expr, SqlType)>> = values
        .iter()
        .map(|value| match as_function_cast(value) {
            Some((inner, data_type)) if !literal_only || is_inlined_literal(inner) => {
                invert_pin_type(data_type, dialect).map(|ty| (inner, ty))
            }
            _ => None,
        })
        .collect();

    // Case A — every branch is a cast: the renderer's uniform result-pin. Require one consistent type.
    if casts.iter().all(Option::is_some) {
        let ty = casts[0].as_ref().expect("all cast").1.clone();
        if casts
            .iter()
            .any(|entry| entry.as_ref().expect("all cast").1 != ty)
        {
            return Err(not_yet(
                "CASE/COALESCE/NULLIF branches cast to differing types",
            ));
        }
        let args = casts
            .iter()
            .map(|entry| lower(entry.as_ref().expect("all cast").0, dialect))
            .collect::<Result<_, _>>()?;
        return Ok((args, Some(ty)));
    }

    // Case B — not a uniform pin. A cast on only some branches is tolerated only when it is PostgreSQL's
    // redundant `::type` deparse of a bare literal (`(0)::bigint`); anything else is a genuine mix.
    for (value, cast) in values.iter().zip(&casts) {
        if cast.is_some() && !is_redundant_double_colon_literal(value) {
            return Err(not_yet(
                "CASE/COALESCE/NULLIF with mixed cast and un-cast branches",
            ));
        }
    }
    // Every cast branch is a redundant `::` literal (deparse noise); lower verbatim — `lower` strips it.
    let args = values
        .iter()
        .map(|value| lower(value, dialect))
        .collect::<Result<_, _>>()?;
    Ok((args, None))
}

/// Whether `value` is PostgreSQL's `pg_get_viewdef` redundant `::type` cast on a bare literal
/// (`(0)::bigint`) — deparse noise a caller can strip to the bare literal, not a result-pin.
fn is_redundant_double_colon_literal(value: &Expr) -> bool {
    matches!(
        value,
        Expr::Cast { kind: CastKind::DoubleColon, expr, data_type, .. }
            if redundant_cast_literal(strip_nested(expr), data_type).is_some()
    )
}

/// If `expr` is a per-branch result-pin cast, returns `(inner, ty)`; else `None`. Recognizes both the
/// renderer's function-style `CAST(<inner> AS <ty>)` and PostgreSQL's `pg_get_viewdef` `::` spelling
/// (`(<inner>)::ty`) — the form a `CASE`/`COALESCE`/`NULLIF` branch pin arrives in when read from the
/// catalog. (`::` parses only on PostgreSQL/Generic, so accepting it here adds no cross-dialect ambiguity.)
fn as_function_cast(expr: &Expr) -> Option<(&Expr, &DataType)> {
    match expr {
        Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            format: None,
            array: false,
        } => Some((expr, data_type)),
        _ => None,
    }
}

/// Whether `expr` is an inlined SQL literal (a bare value, or a signed numeric literal `-5` which parses
/// as unary minus over the magnitude) — the only operand kind `COALESCE`/`NULLIF` per-branch-casts.
fn is_inlined_literal(expr: &Expr) -> bool {
    match strip_nested(expr) {
        Expr::Value(_) => true,
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => {
            matches!(strip_nested(expr), Expr::Value(value) if matches!(&value.value, Value::Number(_, _)))
        }
        _ => false,
    }
}

/// Lowers `EXTRACT(<field> FROM <operand>)`. `SECOND` is the fractional-seconds [`ExprNode::ExtractSecond`]
/// (bare, no `FLOOR`); every other field is the integer [`ExprNode::Extract`], whose operand may be wrapped
/// `(… AT TIME ZONE '<tz>')`.
fn lower_extract(
    field: &DateTimeField,
    operand: &Expr,
    result: Option<SqlType>,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    if *field == DateTimeField::Second {
        // Bare `EXTRACT(SECOND …)` (no surrounding `FLOOR`) is the fractional-seconds node; its operand
        // is never `AT TIME ZONE`-wrapped by the renderer.
        return Ok(ExprNode::ExtractSecond {
            operand: b(lower(operand, dialect)?),
            result,
        });
    }
    let field = map_date_field(field).ok_or_else(|| not_yet(format!("EXTRACT field `{field}`")))?;
    let (operand, timezone) = lower_extract_operand(operand, dialect)?;
    Ok(ExprNode::Extract {
        field,
        operand: b(operand),
        result,
        timezone,
    })
}

/// Lowers `FLOOR(EXTRACT(SECOND FROM <operand>))` — the whole-seconds [`ExprNode::Extract`] for the
/// `Second` field (the renderer floors PostgreSQL's fractional `EXTRACT(SECOND …)`).
fn lower_floored_second(
    inner: &Expr,
    result: Option<SqlType>,
    dialect: SqlDialect,
) -> Result<ExprNode, ReadError> {
    match inner {
        Expr::Extract {
            field: DateTimeField::Second,
            expr,
            syntax: ExtractSyntax::From,
        } => {
            let (operand, timezone) = lower_extract_operand(expr, dialect)?;
            Ok(ExprNode::Extract {
                field: DateField::Second,
                operand: b(operand),
                result,
                timezone,
            })
        }
        other => Err(not_yet(format!("FLOOR around `{other}`"))),
    }
}

/// Lowers `date_trunc('<unit>', <operand>[, '<tz>'])` (PostgreSQL only) into [`ExprNode::DateTrunc`].
/// The unit and the optional 3-argument timezone are string literals (not the `AT TIME ZONE` operand
/// wrapper `EXTRACT` uses).
fn lower_date_trunc(function: &Function, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    let values = unnamed_args(function)?;
    let (unit, operand, tz) = match values.as_slice() {
        [unit, operand] => (unit, operand, None),
        [unit, operand, tz] => (unit, operand, Some(tz)),
        _ => return Err(not_yet(format!("date_trunc call `{function}`"))),
    };
    let unit = date_trunc_unit(unit)?;
    let timezone = tz.map(|tz| string_literal(tz)).transpose()?;
    Ok(ExprNode::DateTrunc {
        unit,
        operand: b(lower(operand, dialect)?),
        timezone,
    })
}

/// Maps a `date_trunc` unit string literal (as `DateField::trunc_literal` spells it) back to its
/// [`DateField`].
fn date_trunc_unit(expr: &Expr) -> Result<DateField, ReadError> {
    let literal = string_literal(expr)?;
    match literal.as_str() {
        "year" => Ok(DateField::Year),
        "month" => Ok(DateField::Month),
        "day" => Ok(DateField::Day),
        "hour" => Ok(DateField::Hour),
        "minute" => Ok(DateField::Minute),
        "second" => Ok(DateField::Second),
        other => Err(not_yet(format!("date_trunc unit `{other}`"))),
    }
}

/// Reads a single-quoted string literal's content; any other expression is outside the grammar.
/// PostgreSQL's `pg_get_viewdef` deparses a bare string argument (e.g. a `date_trunc` unit) with a
/// redundant `::text` cast (`'day'::text`) — that no-op text cast is peeled first.
fn string_literal(expr: &Expr) -> Result<String, ReadError> {
    let expr = match strip_nested(expr) {
        Expr::Cast {
            kind: CastKind::DoubleColon,
            expr,
            data_type,
            ..
        } if is_unbounded_text_type(data_type) => strip_nested(expr),
        other => other,
    };
    match expr {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) => Ok(text.clone()),
            other => Err(not_yet(format!("non-string literal `{other}`"))),
        },
        other => Err(not_yet(format!("non-literal `{other}`"))),
    }
}

/// Maps a parsed [`DateTimeField`] to the neutral [`DateField`]; `None` for a field outside the neutral
/// set (`Second` is handled by the caller as the fractional node).
fn map_date_field(field: &DateTimeField) -> Option<DateField> {
    match field {
        DateTimeField::Year => Some(DateField::Year),
        DateTimeField::Month => Some(DateField::Month),
        DateTimeField::Day => Some(DateField::Day),
        DateTimeField::Hour => Some(DateField::Hour),
        DateTimeField::Minute => Some(DateField::Minute),
        DateTimeField::Second => Some(DateField::Second),
        _ => None,
    }
}

/// Lowers an `EXTRACT`/`date_trunc` operand, recovering the `(<operand> AT TIME ZONE '<tz>')` wrapper the
/// renderer emits for the timezone-explicit form into a `Some(tz)` timezone (else `None`).
fn lower_extract_operand(
    operand: &Expr,
    dialect: SqlDialect,
) -> Result<(ExprNode, Option<String>), ReadError> {
    if let Expr::AtTimeZone {
        timestamp,
        time_zone,
    } = strip_nested(operand)
        && let Expr::Value(value) = strip_nested(time_zone)
        && let Value::SingleQuotedString(tz) = &value.value
    {
        return Ok((lower(timestamp, dialect)?, Some(tz.clone())));
    }
    Ok((lower(operand, dialect)?, None))
}

// ===== single-SELECT view-body lowering =====

/// Lowers a `SELECT` (with its enclosing [`Query`]'s `ORDER BY` / `LIMIT` / `OFFSET`, which attach to the
/// query, not the select) into a [`ViewQueryModel`].
///
/// `output_names` is a declared column list (a `CREATE VIEW (cols)` or a `WITH cte (cols)` list). When
/// `column_list_is_fallback` is `false` (a single-`SELECT` body) the list is **authoritative** — it names
/// the outputs positionally, overriding any inner projection alias, exactly as SQL does. When `true` (a
/// **set-operation arm**) the list is only a **fallback**: each projection keeps its own alias / bare-column
/// name (needed for the arm's internal clauses, e.g. a per-arm `ORDER BY <alias>`), and the list names only
/// an otherwise-unnamed projection — the un-aliased anchor of a recursive CTE (`SELECT 1 UNION ALL …`) or
/// any un-aliased set arm.
fn lower_select(
    select: &Select,
    output_names: Option<&[String]>,
    column_list_is_fallback: bool,
    dialect: SqlDialect,
) -> Result<ViewQueryModel, ReadError> {
    // A select-level clause `ViewQueryModel` cannot represent must fail loudly, not be dropped: silently
    // ignoring, say, a `QUALIFY`/`PREWHERE` would re-render SQL with a different result set. (The renderer
    // emits none of these; they arrive only from externally-authored SQL.) The query-level clauses
    // (`ORDER BY`/`LIMIT`, `WITH`, `FETCH`/`FOR UPDATE`) are checked by the [`lower_body`] caller — a bare
    // set operand has no `Query` of its own, so `order_by`/`limit`/`offset` here stay their default empty.
    reject_unsupported_select_clauses(select)?;

    let distinct = match &select.distinct {
        None => false,
        Some(Distinct::Distinct) => true,
        Some(Distinct::On(_)) => return Err(not_yet("SELECT DISTINCT ON (…)")),
        Some(Distinct::All) => false,
    };

    // Projection. A column-listed view supplies names positionally (its projections are un-aliased);
    // otherwise each projection is named by its `AS` alias, or a bare column by its name.
    if let Some(names) = output_names
        && names.len() != select.projection.len()
    {
        return Err(ReadError::Unexpected(format!(
            "view column list names {} outputs but the SELECT projects {}",
            names.len(),
            select.projection.len()
        )));
    }
    let mut projection = Vec::with_capacity(select.projection.len());
    for (index, item) in select.projection.iter().enumerate() {
        // The projected expression, and the name it would carry from its own `AS` alias or bare column.
        let (expr, self_name) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, bare_column_name(expr)),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(fold_ident(alias))),
            SelectItem::Wildcard(_)
            | SelectItem::QualifiedWildcard(..)
            | SelectItem::ExprWithAliases { .. } => {
                return Err(not_yet(format!("projection item `{item}`")));
            }
        };
        // A column list names the outputs positionally. For a single-`SELECT` body it is *authoritative*
        // (SQL uses it even when a projection also carries an inner `AS` alias, so the list wins). For a
        // set-operation arm it is only a *fallback*: the projection's own alias / bare-column name wins so
        // the arm's internal clauses (e.g. a per-arm `ORDER BY <alias>`) still resolve, and the list names
        // only an otherwise-unnamed projection. Without a column list the projection's own name is required
        // (an un-aliased expression with none is outside the grammar).
        let output_name = match (output_names, column_list_is_fallback) {
            (Some(names), false) => names[index].clone(),
            (Some(names), true) => self_name.unwrap_or_else(|| names[index].clone()),
            (None, _) => {
                self_name.ok_or_else(|| not_yet(format!("un-aliased projection `{expr}`")))?
            }
        };
        projection.push(ProjectionItem {
            output_name,
            expr: lower(expr, dialect)?,
        });
    }

    // FROM: no source (`SELECT <consts>`), one named source, or — later phases — a derived table /
    // comma joins.
    let (from, joins) = match select.from.as_slice() {
        [] => (None, Vec::new()),
        [table] => {
            let source = lower_source(&table.relation, dialect)?;
            let joins = table
                .joins
                .iter()
                .map(|join| lower_join(join, dialect))
                .collect::<Result<_, _>>()?;
            (Some(source), joins)
        }
        // Multiple comma-separated `FROM` entries are cross-products the IR does not model yet.
        _ => return Err(not_yet("comma-separated FROM (implicit cross join)")),
    };

    let filter = select
        .selection
        .as_ref()
        .map(|e| lower(e, dialect))
        .transpose()?;

    let group_by = match &select.group_by {
        GroupByExpr::Expressions(exprs, modifiers) if modifiers.is_empty() => exprs
            .iter()
            .map(|e| lower(e, dialect))
            .collect::<Result<_, _>>()?,
        GroupByExpr::Expressions(_, _) => return Err(not_yet("GROUP BY with modifiers")),
        GroupByExpr::All(_) => return Err(not_yet("GROUP BY ALL")),
    };

    let having = select
        .having
        .as_ref()
        .map(|e| lower(e, dialect))
        .transpose()?;

    // `order_by`/`limit`/`offset` are query-level: the [`lower_body`] caller attaches them for a top-level
    // or parenthesized select, and a bare set operand has none. They default empty here.
    Ok(ViewQueryModel {
        distinct,
        projection,
        from,
        joins,
        filter,
        group_by,
        having,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        dependencies: Vec::new(),
    })
}

/// Binds the unqualified column references a single-source view-body scope carries to that scope's sole
/// source alias, turning each [`ExprNode::BareColumn`] into an [`ExprNode::Column`].
///
/// A deparser (PostgreSQL's `pg_get_viewdef`, and others) drops the alias qualifier from a column when the
/// enclosing `SELECT` has a single source — the reference is unambiguous — so a bare `col` arrives where
/// the forward path (which always qualifies) rendered `<alias>.col`. Re-binding it makes a squealy-published
/// single-source view's reconstructed body compare structurally equal to the crate model instead of churning
/// a `CREATE OR REPLACE VIEW` every plan. Applied only to view bodies (via [`lower_query`] /
/// [`lower_create_view`]), never to the scalar-expression readers, whose bare columns (constraint /
/// generated / index expressions) are genuinely unqualified.
///
/// Resolution is per **scope**: each `SELECT` resolves its own bare columns against its own single source,
/// and a nested scope — a set-operation arm, a `WITH` CTE or body, a derived-table subquery, or a
/// scalar/`IN`/`EXISTS` subquery inside an expression — resolves against its own sources, not the enclosing
/// one. A deparser qualifies a cross-scope (correlated) reference, so a surviving bare column always belongs
/// to its own scope's single source; a multi-source (join) scope keeps its columns as-is (already qualified,
/// and an unqualified one has no unique alias to bind to). Idempotent: a column already in
/// [`ExprNode::Column`] form is left untouched.
fn resolve_single_source_columns(body: &mut ViewBody) {
    match body {
        ViewBody::Select(query) => resolve_query_columns(query),
        ViewBody::Set { left, right, .. } => {
            resolve_single_source_columns(left);
            resolve_single_source_columns(right);
            // A set's own `ORDER BY` references the compound output columns by name (no source scope), so it
            // is left as the deparser emits it — matching how the renderer emits a set-level `ORDER BY`.
        }
        ViewBody::With { ctes, body, .. } => {
            for cte in ctes {
                resolve_single_source_columns(&mut cte.body);
            }
            resolve_single_source_columns(body);
        }
    }
}

/// Resolves one `SELECT` scope (see [`resolve_single_source_columns`]).
fn resolve_query_columns(query: &mut ViewQueryModel) {
    // This scope's sole source alias, when it is single-source (one `FROM`, no joins).
    let alias = match (&query.from, query.joins.is_empty()) {
        (Some(from), true) => Some(from.alias().to_owned()),
        _ => None,
    };
    let alias = alias.as_deref();

    // A projection expression and a `WHERE`/`ON` predicate reference only source columns, so a bare column
    // there is always a source column.
    for item in &mut query.projection {
        resolve_expr_columns(&mut item.expr, alias);
    }
    // A derived `FROM`/`JOIN` source is its own scope; a named source binds no columns.
    if let Some(from) = &mut query.from {
        resolve_source_columns(from);
    }
    for join in &mut query.joins {
        resolve_source_columns(&mut join.source);
        if let Some(on) = &mut join.on {
            // A join condition spans every source, so its columns stay as the deparser qualified them.
            resolve_expr_columns(on, None);
        }
    }
    if let Some(filter) = &mut query.filter {
        resolve_expr_columns(filter, alias);
    }

    // `GROUP BY`/`HAVING`/`ORDER BY` can name a projection **output alias** instead of a source column.
    // This matters only when that output is a *computed* expression (`(amount * 2) AS total`): the
    // renderer emits `ORDER BY total` bare (it cannot re-derive the expression), so a top-level bare term
    // naming such an output is a genuine alias reference and is left as-is. A bare term that names a
    // plain-column projection (`q.id AS id`) is *not* left bare — the deparser merely dequalified the
    // source column `q.id` to `id`, and the desired model qualifies it, so it must be bound to the scope
    // alias like any other source column. (A bare column *nested* in an expression — `ORDER BY total + 1`
    // — is a source column, not an alias, so the guard is only for the top-level term.)
    let computed_aliases: Vec<&str> = query
        .projection
        .iter()
        .filter(|item| {
            !matches!(
                item.expr,
                ExprNode::Column { .. } | ExprNode::BareColumn { .. }
            )
        })
        .map(|item| item.output_name.as_str())
        .collect();
    for expr in &mut query.group_by {
        resolve_clause_term_columns(expr, alias, &computed_aliases);
    }
    if let Some(having) = &mut query.having {
        resolve_clause_term_columns(having, alias, &computed_aliases);
    }
    for order in &mut query.order_by {
        resolve_clause_term_columns(&mut order.expr, alias, &computed_aliases);
    }
}

/// Resolves a `GROUP BY`/`HAVING`/`ORDER BY` term, leaving a top-level bare column that names a
/// *computed* projection output alias untouched (the renderer emits it bare — see
/// [`resolve_query_columns`]); anything else resolves like an ordinary scope expression.
fn resolve_clause_term_columns(
    expr: &mut ExprNode,
    alias: Option<&str>,
    computed_aliases: &[&str],
) {
    if let ExprNode::BareColumn { column } = expr
        && computed_aliases.contains(&column.as_str())
    {
        return;
    }
    resolve_expr_columns(expr, alias);
}

/// Recurses into a derived-table source's own scope; a named source binds no columns of its own.
fn resolve_source_columns(source: &mut SourceItem) {
    match source {
        SourceItem::Named(_) => {}
        SourceItem::Derived { query, .. } => resolve_single_source_columns(query),
    }
}

/// Resolves the bare columns of one expression against `alias` (this scope's single source, or `None`),
/// recursing into child expressions in the same scope and into nested subqueries as new scopes. Exhaustive
/// over [`ExprNode`] so a new variant is a compile error here rather than a silently-unresolved column.
fn resolve_expr_columns(expr: &mut ExprNode, alias: Option<&str>) {
    match expr {
        ExprNode::BareColumn { column } => {
            if let Some(alias) = alias {
                *expr = ExprNode::Column {
                    alias: alias.to_owned(),
                    column: std::mem::take(column),
                };
            }
        }
        // Nested query scopes resolve against their own sources.
        ExprNode::ScalarSubquery(query) => resolve_query_columns(query),
        ExprNode::Exists { subquery, .. } => resolve_query_columns(subquery),
        ExprNode::InSubquery {
            operand, subquery, ..
        } => {
            resolve_expr_columns(operand, alias);
            resolve_query_columns(subquery);
        }
        // Child expressions in the same scope.
        ExprNode::Binary { left, right, .. }
        | ExprNode::Compare { left, right, .. }
        | ExprNode::Logical { left, right, .. }
        | ExprNode::Nullif { left, right, .. } => {
            resolve_expr_columns(left, alias);
            resolve_expr_columns(right, alias);
        }
        ExprNode::Cast { operand, .. }
        | ExprNode::Aggregate { operand, .. }
        | ExprNode::Not(operand)
        | ExprNode::IsNull { operand, .. }
        | ExprNode::Extract { operand, .. }
        | ExprNode::DateTrunc { operand, .. }
        | ExprNode::ExtractSecond { operand, .. } => resolve_expr_columns(operand, alias),
        ExprNode::Like {
            operand, pattern, ..
        } => {
            resolve_expr_columns(operand, alias);
            resolve_expr_columns(pattern, alias);
        }
        ExprNode::In { operand, items, .. } => {
            resolve_expr_columns(operand, alias);
            for item in items {
                resolve_expr_columns(item, alias);
            }
        }
        ExprNode::Between {
            operand, low, high, ..
        } => {
            resolve_expr_columns(operand, alias);
            resolve_expr_columns(low, alias);
            resolve_expr_columns(high, alias);
        }
        ExprNode::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                resolve_expr_columns(arg, alias);
            }
            for part in partition_by {
                resolve_expr_columns(part, alias);
            }
            for order in order_by {
                resolve_expr_columns(&mut order.expr, alias);
            }
        }
        ExprNode::Case { arms, else_, .. } => resolve_case_arm_columns(arms, else_, alias),
        ExprNode::SimpleCase {
            operand,
            arms,
            else_,
            ..
        } => {
            resolve_expr_columns(operand, alias);
            resolve_case_arm_columns(arms, else_, alias);
        }
        ExprNode::Coalesce { args, .. }
        | ExprNode::ScalarFn { args, .. }
        | ExprNode::Function { args, .. } => {
            for arg in args {
                resolve_expr_columns(arg, alias);
            }
        }
        // Leaves — no nested expression, no column to resolve.
        ExprNode::Column { .. } | ExprNode::Literal(_) | ExprNode::Raw(_) | ExprNode::Now => {}
    }
}

/// Resolves every `WHEN`/`THEN` arm and the optional `ELSE` of a `CASE` body against `alias`.
fn resolve_case_arm_columns(
    arms: &mut [CaseArm],
    else_: &mut Option<Box<ExprNode>>,
    alias: Option<&str>,
) {
    for arm in arms {
        resolve_expr_columns(&mut arm.when, alias);
        resolve_expr_columns(&mut arm.then, alias);
    }
    if let Some(else_) = else_ {
        resolve_expr_columns(else_, alias);
    }
}

/// Rejects the query-level clauses a [`ViewBody`] does not represent, so they surface as
/// [`ReadError::NotYetLowered`] rather than being silently discarded (which would re-render different
/// SQL). The clauses this path *does* lower — `WITH` (rejected separately, 2.0c) and
/// `ORDER BY`/`LIMIT`/`OFFSET` — are handled by [`lower_body`]; everything else a [`Query`] can attach is
/// enumerated here. Runs once per `Query` (the outer query and each parenthesized operand).
fn reject_unsupported_query_clauses(query: &Query) -> Result<(), ReadError> {
    if query.fetch.is_some() {
        return Err(not_yet("FETCH clause"));
    }
    if !query.locks.is_empty() {
        return Err(not_yet("row-locking clause (FOR UPDATE / FOR SHARE)"));
    }
    if query.for_clause.is_some() {
        return Err(not_yet("FOR clause (FOR XML / FOR JSON)"));
    }
    if query.settings.is_some() {
        return Err(not_yet("SETTINGS clause"));
    }
    if query.format_clause.is_some() {
        return Err(not_yet("FORMAT clause"));
    }
    if !query.pipe_operators.is_empty() {
        return Err(not_yet("pipe operators"));
    }
    Ok(())
}

/// Rejects the select-level clauses a [`ViewQueryModel`] does not represent (the analog of
/// [`reject_unsupported_query_clauses`] for a `SELECT`). The clauses this path *does* lower —
/// `DISTINCT`/projection/`FROM`/`WHERE`/`GROUP BY`/`HAVING` — are handled by [`lower_select`]; everything
/// else a [`Select`] can attach is enumerated here.
fn reject_unsupported_select_clauses(select: &Select) -> Result<(), ReadError> {
    if select.flavor != SelectFlavor::Standard {
        return Err(not_yet("non-standard SELECT flavor (FROM-first)"));
    }
    if select.top.is_some() {
        return Err(not_yet("TOP clause"));
    }
    if select.into.is_some() {
        return Err(not_yet("SELECT … INTO"));
    }
    if !select.lateral_views.is_empty() {
        return Err(not_yet("LATERAL VIEW"));
    }
    if select.prewhere.is_some() {
        return Err(not_yet("PREWHERE clause"));
    }
    if !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
    {
        return Err(not_yet("CLUSTER BY / DISTRIBUTE BY / SORT BY clause"));
    }
    if !select.connect_by.is_empty() {
        return Err(not_yet("CONNECT BY clause"));
    }
    if !select.named_window.is_empty() {
        return Err(not_yet("named WINDOW clause"));
    }
    if select.qualify.is_some() {
        return Err(not_yet("QUALIFY clause"));
    }
    if select.value_table_mode.is_some() {
        return Err(not_yet("value-table mode (AS STRUCT / AS VALUE)"));
    }
    if !select.optimizer_hints.is_empty() {
        return Err(not_yet("optimizer hints"));
    }
    if select.select_modifiers.is_some() {
        return Err(not_yet("SELECT modifiers"));
    }
    if select.exclude.is_some() {
        return Err(not_yet("EXCLUDE clause"));
    }
    Ok(())
}

/// The output column name a bare-column projection carries when un-aliased (`SELECT a` → `a`,
/// `SELECT t.a` → `a`); `None` for any expression that needs an explicit alias to be named.
fn bare_column_name(expr: &Expr) -> Option<String> {
    match strip_nested(expr) {
        Expr::Identifier(ident) => Some(fold_ident(ident)),
        Expr::CompoundIdentifier(parts) => parts.last().map(fold_ident),
        _ => None,
    }
}

/// Lowers a `FROM`/`JOIN` [`TableFactor`] into a [`SourceItem`]. A named `Table` becomes
/// [`SourceItem::Named`] (schema from a multi-part name, alias from the `AS`); a `Derived` table (a
/// parenthesized subquery) recursively lowers its body via [`lower_query`] into [`SourceItem::Derived`].
/// A table function, `UNNEST`, `LATERAL`/sampled derived table, or hinted/versioned table is a later
/// phase.
fn lower_source(factor: &TableFactor, dialect: SqlDialect) -> Result<SourceItem, ReadError> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args: None,
            with_hints,
            version: None,
            with_ordinality: false,
            partitions,
            json_path: None,
            sample: None,
            index_hints,
        } if with_hints.is_empty() && partitions.is_empty() && index_hints.is_empty() => {
            let (schema, name) = split_object_name(name)?;
            let alias = table_alias(alias.as_ref())?;
            Ok(SourceItem::Named(SourceRef {
                schema,
                name,
                alias,
            }))
        }
        // A derived table: `(<subquery>) AS <alias>`. The alias is required and must be a bare unquoted
        // identifier (the renderer emits it unquoted); its body lowers like any other view query, so a
        // nested `WITH`/set-op/un-lowerable body surfaces as `NotYetLowered`. `LATERAL` and table-sample
        // modifiers carry semantics the neutral model cannot hold.
        TableFactor::Derived {
            lateral: false,
            subquery,
            alias,
            sample: None,
        } => {
            let alias = table_alias(alias.as_ref())?;
            let query = lower_query(subquery, dialect)?;
            Ok(SourceItem::Derived {
                query: Box::new(query),
                alias,
            })
        }
        // A `LATERAL`/sampled derived table, table function, `UNNEST`, or hinted/versioned table.
        other => Err(not_yet(format!("FROM source `{other}`"))),
    }
}

/// Splits a source's [`ObjectName`] into an optional schema and the table name (`fold_ident` applied so
/// unquoted names match the renderer's re-quoting). Only bare or two-part names are lowered.
fn split_object_name(name: &ObjectName) -> Result<(Option<String>, String), ReadError> {
    let idents: Option<Vec<_>> = name.0.iter().map(|part| part.as_ident()).collect();
    let idents = idents.ok_or_else(|| not_yet(format!("non-identifier source name `{name}`")))?;
    match idents.as_slice() {
        [table] => Ok((None, fold_ident(table))),
        [schema, table] => Ok((Some(fold_ident(schema)), fold_ident(table))),
        _ => Err(not_yet(format!("source name with {} parts", idents.len()))),
    }
}

/// The required source alias — the renderer always binds `<source> AS <alias>` so columns qualify with
/// the alias. A missing alias, or one carrying column aliases (`t (a, b)`), is outside that grammar.
///
/// The renderer emits the alias UNQUOTED (`AS {alias}`) and every qualified column as `{alias}.col`, so
/// the alias must be a bare identifier that re-emits and re-parses unquoted unchanged — squealy's
/// generated `q0_0`-style aliases are. An alias that needs quotes (special characters, mixed case — an
/// unquoted identifier parses case-folded — or a reserved word) would re-render as invalid or different
/// SQL, so it is left `NotYetLowered` (only externally-authored views use such aliases).
fn table_alias(alias: Option<&TableAlias>) -> Result<String, ReadError> {
    match alias {
        Some(alias) if alias.columns.is_empty() && alias.at.is_none() => {
            let name = fold_ident(&alias.name);
            if is_bare_unquoted_alias(&name) {
                Ok(name)
            } else {
                Err(not_yet(format!(
                    "source alias `{}` is not a bare unquoted identifier",
                    alias.name
                )))
            }
        }
        Some(_) => Err(not_yet("source alias with column aliases")),
        None => Err(not_yet("un-aliased FROM source")),
    }
}

/// Whether `name` re-emits (and re-parses) unchanged as an unquoted SQL identifier: ASCII, starts with a
/// lowercase letter or `_`, continues with lowercase letters / digits / `_`, and is not a keyword. An
/// unquoted identifier parses case-folded to lowercase, so a mixed-case name would not round-trip; a
/// special character would need quotes; a keyword unquoted is a syntax error. Matches squealy's generated
/// `q0_0`-style aliases.
fn is_bare_unquoted_alias(name: &str) -> bool {
    let mut chars = name.chars();
    let starts = matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_lowercase());
    let regular = starts && chars.all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit());
    // `ALL_KEYWORDS` is uppercase and sorted; reject any alias that collides with a keyword (conservative
    // — some are non-reserved, but squealy never emits a keyword alias).
    regular
        && sqlparser::keywords::ALL_KEYWORDS
            .binary_search(&name.to_ascii_uppercase().as_str())
            .is_err()
}

/// Lowers a single [`Join`] into a [`JoinItem`], mapping the join operator to a [`JoinKind`] and the
/// `ON` constraint (or none, for `CROSS JOIN`). `USING`/`NATURAL` joins are a later phase.
fn lower_join(join: &Join, dialect: SqlDialect) -> Result<JoinItem, ReadError> {
    let source = lower_source(&join.relation, dialect)?;
    let (kind, constraint) = match &join.join_operator {
        JoinOperator::Inner(constraint) => (JoinKind::Inner, constraint),
        JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
            (JoinKind::Left, constraint)
        }
        JoinOperator::Right(constraint) | JoinOperator::RightOuter(constraint) => {
            (JoinKind::Right, constraint)
        }
        JoinOperator::FullOuter(constraint) => (JoinKind::Full, constraint),
        JoinOperator::CrossJoin(constraint) => (JoinKind::Cross, constraint),
        other => return Err(not_yet(format!("join operator `{other:?}`"))),
    };
    let on = match (kind, constraint) {
        // A standard `CROSS JOIN` is unconditioned. Some dialects accept `CROSS JOIN … ON`/`USING`, whose
        // predicate the neutral `Cross` join cannot hold — reject it rather than silently drop it (which
        // would re-render an unconstrained Cartesian product).
        (JoinKind::Cross, JoinConstraint::None) => None,
        (JoinKind::Cross, _) => return Err(not_yet("CROSS JOIN with an ON/USING condition")),
        (_, JoinConstraint::On(expr)) => Some(lower(expr, dialect)?),
        // `USING`/`NATURAL` have no neutral node yet; a conditionless non-cross join is unexpected.
        (_, JoinConstraint::Using(_)) => return Err(not_yet("JOIN … USING (…)")),
        (_, JoinConstraint::Natural) => return Err(not_yet("NATURAL JOIN")),
        (_, JoinConstraint::None) => {
            return Err(not_yet("non-CROSS join without an ON condition"));
        }
    };
    Ok(JoinItem { kind, source, on })
}

/// Lowers a query's `ORDER BY` into [`OrderItem`]s (expression + optional `ASC`/`DESC` + optional
/// `NULLS FIRST`/`LAST`). `ORDER BY ALL` and ClickHouse `WITH FILL` are outside the grammar.
fn lower_order_by(
    order_by: Option<&OrderBy>,
    dialect: SqlDialect,
) -> Result<Vec<OrderItem>, ReadError> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    let exprs = match &order_by.kind {
        OrderByKind::Expressions(exprs) => exprs,
        OrderByKind::All(_) => return Err(not_yet("ORDER BY ALL")),
    };
    exprs
        .iter()
        .map(|order| {
            if order.with_fill.is_some() {
                return Err(not_yet("ORDER BY … WITH FILL"));
            }
            let direction = match order.options.asc {
                Some(true) => Some(OrderDirection::Asc),
                Some(false) => Some(OrderDirection::Desc),
                None => None,
            };
            let nulls = match order.options.nulls_first {
                Some(true) => Some(OrderNulls::First),
                Some(false) => Some(OrderNulls::Last),
                None => None,
            };
            Ok(OrderItem {
                expr: lower(&order.expr, dialect)?,
                direction,
                nulls,
            })
        })
        .collect()
}

/// MySQL's documented "all rows" limit — `u64::MAX` — which its renderer emits as the sentinel limit for
/// an offset-*without*-limit query (MySQL has no bare `OFFSET`). Matched as a string so it recovers even on
/// a 32-bit `usize` (where it would overflow [`integer_literal`]).
const MYSQL_NO_LIMIT_SENTINEL: &str = "18446744073709551615";

/// Lowers a query's `LIMIT`/`OFFSET` into the neutral `(limit, offset)` pair. Only a plain integer
/// literal count is lowered; `LIMIT ALL`, an expression bound, or the ClickHouse `BY` clause is a later
/// phase.
fn lower_limit_offset(
    limit_clause: Option<&LimitClause>,
    dialect: SqlDialect,
) -> Result<(Option<usize>, Option<usize>), ReadError> {
    match limit_clause {
        None => Ok((None, None)),
        Some(LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            if !limit_by.is_empty() {
                return Err(not_yet("LIMIT … BY (…)"));
            }
            let offset = offset
                .as_ref()
                .map(|offset| integer_literal(&offset.value))
                .transpose()?;
            // MySQL renders an offset-only view as `LIMIT <u64::MAX> OFFSET n` (it has no bare `OFFSET`);
            // that max-u64 limit is the "all rows" sentinel, not a real bound, so recover it to
            // `limit: None` — else an offset-only view would carry `Some(u64::MAX)` and never re-plan to
            // empty. Gate on an `OFFSET` being present: a *bare* `LIMIT <u64::MAX>` is the renderer's output
            // for a genuine — if absurd — `Some(usize::MAX)` limit, which must round-trip unchanged.
            let limit = match limit {
                Some(expr)
                    if dialect == SqlDialect::Mysql
                        && offset.is_some()
                        && is_number(expr, MYSQL_NO_LIMIT_SENTINEL) =>
                {
                    None
                }
                Some(expr) => Some(integer_literal(expr)?),
                None => None,
            };
            Ok((limit, offset))
        }
        // MySQL's `LIMIT <offset>, <limit>`; the renderer emits the `LIMIT … OFFSET …` form instead.
        Some(LimitClause::OffsetCommaLimit { .. }) => Err(not_yet("LIMIT <offset>, <limit> form")),
    }
}

/// Reads a plain non-negative integer literal into a `usize` (a `LIMIT`/`OFFSET` count); a non-integer or
/// out-of-range bound is outside the grammar.
fn integer_literal(expr: &Expr) -> Result<usize, ReadError> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(number, false) => number
                .parse::<usize>()
                .map_err(|_| not_yet(format!("non-integer or out-of-range bound `{number}`"))),
            other => Err(not_yet(format!("non-numeric bound `{other}`"))),
        },
        other => Err(not_yet(format!("non-literal bound `{other}`"))),
    }
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

    /// A qualified `q0_0.<column>` — a view body binds every column to a source alias.
    fn col(column: &str) -> ExprNode {
        ExprNode::Column {
            alias: "q0_0".to_owned(),
            column: column.to_owned(),
        }
    }

    /// Lowers a `CREATE VIEW` / bare `SELECT` statement into its [`ViewBody`], panicking on a parse
    /// error so a test asserts against the lowering outcome directly.
    fn low_body(sql: &str, dialect: SqlDialect) -> Result<ViewBody, ReadError> {
        use sqlparser::ast::Statement;
        let statements = crate::parse_sql(sql, dialect).expect("parses");
        match statements.as_slice() {
            [Statement::CreateView(create_view)] => lower_create_view(create_view, dialect),
            [Statement::Query(query)] => lower_query(query, dialect),
            other => panic!("expected one CREATE VIEW / SELECT statement, got: {other:?}"),
        }
    }

    /// Like [`low_body`] but unwraps a single-`SELECT` body into its [`ViewQueryModel`] — most lowering
    /// tests assert against a `SELECT`. A set-operation body panics (those tests use [`low_body`]).
    fn low_query(sql: &str, dialect: SqlDialect) -> Result<ViewQueryModel, ReadError> {
        low_body(sql, dialect).map(|body| match body {
            ViewBody::Select(select) => *select,
            ViewBody::Set { .. } => panic!("expected a single-SELECT body, got a set operation"),
            ViewBody::With { .. } => panic!("expected a single-SELECT body, got a `WITH` prelude"),
        })
    }

    /// Unwraps a nested [`ViewBody`] (a derived-table body / set operand) to its single `SELECT`.
    fn select_of(body: &ViewBody) -> &ViewQueryModel {
        match body {
            ViewBody::Select(select) => select,
            ViewBody::Set { .. } => panic!("expected a single-SELECT body, got a set operation"),
            ViewBody::With { .. } => panic!("expected a single-SELECT body, got a `WITH` prelude"),
        }
    }

    #[test]
    fn single_source_view_columns_bind_to_the_source_alias() {
        // PostgreSQL's `pg_get_viewdef` dequalifies an unambiguous single-source column, so the reader
        // re-binds each bare column of a single-source view body to the sole source alias — in the
        // projection, `WHERE`, and a `GROUP BY`/`ORDER BY` term naming a plain-column output (the deparser
        // dequalified the source column; it is not an alias reference).
        let col = |c: &str| ExprNode::Column {
            alias: "q0_0".to_owned(),
            column: c.to_owned(),
        };
        let query = low_query(
            "SELECT id FROM \"public\".\"users\" q0_0 WHERE active = true GROUP BY id ORDER BY id",
            SqlDialect::Postgres,
        )
        .unwrap();
        assert_eq!(query.projection[0].expr, col("id"));
        assert_eq!(
            query.filter,
            Some(ExprNode::Compare {
                op: CompareOp::Equals,
                left: Box::new(col("active")),
                right: Box::new(ExprNode::Literal("TRUE".to_owned())),
            })
        );
        assert_eq!(query.group_by, vec![col("id")]);
        assert_eq!(query.order_by[0].expr, col("id"));

        // A *computed* projection output referenced by a clause is a genuine alias reference: it stays
        // bare (the renderer emits `ORDER BY total`, which cannot be re-derived to the source expression).
        // The bare source column *inside* that projection expression is still bound.
        let query = low_query(
            "SELECT (amount * 2) AS total FROM \"public\".\"t\" q0_0 ORDER BY total",
            SqlDialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            query.order_by[0].expr,
            ExprNode::BareColumn {
                column: "total".to_owned(),
            }
        );
        assert_eq!(
            query.projection[0].expr,
            ExprNode::Binary {
                op: ArithmeticOp::Multiply,
                left: Box::new(col("amount")),
                right: Box::new(ExprNode::Literal("2".to_owned())),
            }
        );
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
    fn pipe_concat_flattens_but_preserves_explicit_nesting() {
        let concat = |args: Vec<ExprNode>| ExprNode::ScalarFn {
            func: ScalarFunc::Concat,
            args,
        };
        // A flat 3-way `||` chain (the renderer's output for a single 3-arg `Concat`) flattens to one flat
        // node, so it re-renders `(a || b || c)` rather than churning into `((a || b) || c)`.
        assert_eq!(
            low("(\"a\" || \"b\" || \"c\")", SqlDialect::Postgres).unwrap(),
            concat(vec![bare("a"), bare("b"), bare("c")]),
        );
        // But an EXPLICITLY nested concat keeps its structure — a parenthesized sub-concat is one arg — so
        // `((a || b) || c)` and `(a || (b || c))` (the render of nested `Concat` models) round-trip nested.
        assert_eq!(
            low("((\"a\" || \"b\") || \"c\")", SqlDialect::Postgres).unwrap(),
            concat(vec![concat(vec![bare("a"), bare("b")]), bare("c")]),
        );
        assert_eq!(
            low("(\"a\" || (\"b\" || \"c\"))", SqlDialect::Postgres).unwrap(),
            concat(vec![bare("a"), concat(vec![bare("b"), bare("c")])]),
        );
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
        // PostgreSQL's `pg_get_viewdef` deparses the same float casts in the `::` form
        // `(a)::double precision / (b)::double precision`; the divide idiom must peel that too.
        assert_eq!(
            low(
                "((\"a\")::double precision / (\"b\")::double precision)",
                SqlDialect::Postgres
            )
            .unwrap(),
            expected
        );
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
        // `CHAR_LENGTH` is character length everywhere → folds to the neutral node.
        assert_eq!(
            low("CHAR_LENGTH(`s`)", SqlDialect::Mysql).unwrap(),
            ExprNode::ScalarFn {
                func: ScalarFunc::Length,
                args: vec![bare("s")],
            }
        );
        // MySQL `LENGTH` is *bytes* and must not fold to the neutral `ScalarFn::Length` (which
        // re-renders as `CHAR_LENGTH`, changing multibyte semantics). It lowers instead to a general
        // `Function` node that renders `length(...)` verbatim, preserving the byte-length meaning.
        assert_eq!(
            low("LENGTH(`s`)", SqlDialect::Mysql).unwrap(),
            ExprNode::Function {
                name: "length".to_string(),
                args: vec![bare("s")],
            }
        );
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
        // (different semantics from the `||` the neutral `ScalarFn::Concat` re-renders as there), so it
        // must not fold to that node. It lowers instead to a general `Function` node that renders
        // `concat(...)` verbatim, preserving PostgreSQL's NULL-ignoring semantics.
        assert_eq!(
            low("CONCAT(\"a\", \"b\")", SqlDialect::Postgres).unwrap(),
            ExprNode::Function {
                name: "concat".to_string(),
                args: vec![bare("a"), bare("b")],
            }
        );
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
        // A `::text` (or any `::`) cast around a NON-literal operand is ambiguous without the operand's
        // type — redundant on a text column, a real conversion on `id::text LIKE '1%'` — so it is NOT
        // stripped; it stays Raw rather than risk dropping a semantic cast.
        assert!(matches!(
            low("(char_length((name)::text) > 0)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low("(id::text ~~ '1%')", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));

        // PostgreSQL deparses a NEGATIVE numeric literal as a string cast: `-5` → `('-5')::integer`.
        assert_eq!(
            low("('-5')::integer", SqlDialect::Postgres).unwrap(),
            low("-5", SqlDialect::Generic).unwrap()
        );
        assert_eq!(
            low("(status = ('-5')::integer)", SqlDialect::Postgres).unwrap(),
            low("status = -5", SqlDialect::Generic).unwrap()
        );
        assert_eq!(
            low("('-1.5')::numeric", SqlDialect::Postgres).unwrap(),
            low("-1.5", SqlDialect::Generic).unwrap()
        );
        // …but a negative fractional string cast to an INTEGER type truncates → stays Raw.
        assert!(matches!(
            low("('-1.5')::integer", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
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
        // `%` lowers to the neutral `Modulo` arithmetic node (same operator on every dialect).
        assert!(matches!(
            low("(\"a\" % 2)", SqlDialect::Postgres),
            Ok(ExprNode::Binary {
                op: ArithmeticOp::Modulo,
                ..
            })
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
        // A subquery in a scalar position.
        assert!(matches!(
            low("(\"a\" IN (SELECT 1))", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A window *frame* is not yet inverted (the simple windows lowering covers carry none).
        assert!(matches!(
            low(
                "ROW_NUMBER() OVER (ORDER BY \"a\" ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn general_functions_lower_to_a_function_node() {
        // A function outside the closed `ScalarFn` set (whose spelling diverges by dialect) lowers to a
        // general, verbatim `Function` node — the name is stored lowercased and the arguments recurse.
        assert_eq!(
            low("md5(\"s\")", SqlDialect::Postgres).unwrap(),
            ExprNode::Function {
                name: "md5".to_string(),
                args: vec![bare("s")],
            }
        );
        assert_eq!(
            low("jsonb_typeof(\"data\")", SqlDialect::Postgres).unwrap(),
            ExprNode::Function {
                name: "jsonb_typeof".to_string(),
                args: vec![bare("data")],
            }
        );
        // The name folds to lowercase (matching PostgreSQL's unquoted deparse). Multiple column
        // arguments are supported.
        assert_eq!(
            low("MD5(\"s\")", SqlDialect::Postgres).unwrap(),
            ExprNode::Function {
                name: "md5".to_string(),
                args: vec![bare("s")],
            }
        );
        assert_eq!(
            low("custom_fn(\"a\", \"b\")", SqlDialect::Postgres).unwrap(),
            ExprNode::Function {
                name: "custom_fn".to_string(),
                args: vec![bare("a"), bare("b")],
            }
        );
        // A wildcard argument (`count(*)`) is still outside the grammar.
        assert!(matches!(
            low("count(*)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A *quoted* function name is NOT lowered — folding its case would change which overload the
        // check calls, so it stays `Raw` (normalized as a string by the backend, preserving the quotes).
        assert!(matches!(
            low("\"MyFunc\"(\"s\")", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A *direct literal argument* is NOT lowered — pg deparses it as `f('x'::text)`, and stripping
        // that synthesized arg cast to converge would rewrite the term the canonical model feeds into
        // `GENERATED`/`CREATE INDEX` DDL, potentially resolving a different overload; it stays `Raw`.
        assert!(matches!(
            low("my_func('x')", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low("my_func(\"data\", 'x'::text)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    // ---- view-body node lowering --------------------------------------------------------------

    #[test]
    fn pg_result_pins_invert_across_the_type_vocabulary() {
        // A result pin's type is the view's output column type — any `SqlType`. PostgreSQL's cast
        // spellings are (mostly) exact, so each renderer-emitted pin type must invert precisely, not just
        // the basic int/float widths. (A view with a `Date`/`Timestamp`/`Decimal`/`Uuid` output column
        // otherwise fails to read.)
        let pin = |ty: &str| {
            let sql = format!("CAST(MAX(q0_0.\"c\") AS {ty})");
            match low(&sql, SqlDialect::Postgres).unwrap() {
                ExprNode::Aggregate { result, .. } => result,
                other => panic!("expected an aggregate, got: {other:?}"),
            }
        };
        assert_eq!(pin("date"), Some(SqlType::Date));
        assert_eq!(
            pin("timestamp(6) with time zone"),
            Some(SqlType::Timestamp {
                tz: true,
                precision: Some(6),
            })
        );
        assert_eq!(
            pin("timestamp"),
            Some(SqlType::Timestamp {
                tz: false,
                precision: None,
            })
        );
        assert_eq!(
            pin("numeric(10,2)"),
            Some(SqlType::Decimal {
                precision: 10,
                scale: 2,
            })
        );
        assert_eq!(pin("uuid"), Some(SqlType::Uuid));
        assert_eq!(pin("bytea"), Some(SqlType::Bytes));
        assert_eq!(pin("smallint"), Some(SqlType::I16));
        // PostgreSQL renders both `String` and `Text` as `text`, and introspection canonicalizes it to
        // `String` — invert to `String` so a String-pinned view compares equal to its introspected form.
        assert_eq!(pin("text"), Some(SqlType::String));
        // The renderer emits `varchar(n)`; `pg_get_viewdef` deparses it as `character varying(n)` — both
        // invert to `Varchar`.
        assert_eq!(pin("varchar(10)"), Some(SqlType::Varchar(10)));
        assert_eq!(pin("character varying(10)"), Some(SqlType::Varchar(10)));
        assert_eq!(pin("char(5)"), Some(SqlType::Char(5)));
        // Bare `numeric` is the 128-bit-integer pin; canonical `I128`.
        assert_eq!(pin("numeric"), Some(SqlType::I128));
    }

    #[test]
    fn pg_double_colon_result_pins_peel_like_the_function_cast() {
        // PostgreSQL's `pg_get_viewdef` deparses a result-pin as the `::` form `(<call>)::type` (whereas
        // the renderer writes the function-style `CAST(<call> AS type)`). Both must peel into the node's
        // `result` so a view read back from the catalog round-trips — the `::` form is what a real PG view
        // introspection feeds `read_view_query`.
        assert_eq!(
            low("(sum(q0_0.\"amount\"))::bigint", SqlDialect::Postgres).unwrap(),
            ExprNode::Aggregate {
                func: AggregateFunc::Sum,
                distinct: false,
                operand: b(col("amount")),
                result: Some(SqlType::I64),
            }
        );
        assert_eq!(
            low(
                "(EXTRACT(YEAR FROM q0_0.\"created\"))::integer",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Extract {
                field: DateField::Year,
                operand: b(col("created")),
                result: Some(SqlType::I32),
                timezone: None,
            }
        );
        // A `::` cast on a bare column is still a general cast, not a pin → not lowered (unchanged).
        assert!(matches!(
            low("(q0_0.\"amount\")::bigint", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        // A redundant literal `::` cast is still recovered (unchanged).
        assert_eq!(low("(0)::numeric", SqlDialect::Postgres).unwrap(), lit("0"));

        // Per-branch `CASE` pins also arrive in the `::` form from `pg_get_viewdef`
        // (`THEN (cnt)::bigint`); `recover_branch_casts` must recognize them so a typed conditional
        // reconstructs (a column branch is cast by the `CASE` renderer whenever `result` is set).
        assert_eq!(
            low(
                "CASE WHEN (q0_0.\"cnt\" > 0) THEN (q0_0.\"cnt\")::bigint ELSE (0)::bigint END",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Case {
                arms: vec![CaseArm {
                    when: b(ExprNode::Compare {
                        op: CompareOp::GreaterThan,
                        left: b(col("cnt")),
                        right: b(lit("0")),
                    }),
                    then: b(col("cnt")),
                }],
                else_: Some(b(lit("0"))),
                result: Some(SqlType::I64),
            }
        );
    }

    #[test]
    fn aggregates_peel_the_cast_result_pin() {
        // `SUM` with a `bigint` pin peels into `result: Some(I64)` (exact on PostgreSQL); `COUNT` with
        // no pin lowers to `result: None`.
        assert_eq!(
            low("CAST(SUM(q0_0.\"amount\") AS bigint)", SqlDialect::Postgres).unwrap(),
            ExprNode::Aggregate {
                func: AggregateFunc::Sum,
                distinct: false,
                operand: b(col("amount")),
                result: Some(SqlType::I64),
            }
        );
        assert_eq!(
            low("COUNT(q0_0.\"id\")", SqlDialect::Postgres).unwrap(),
            ExprNode::Aggregate {
                func: AggregateFunc::Count,
                distinct: false,
                operand: b(col("id")),
                result: None,
            }
        );
        // `DISTINCT` is recovered.
        assert_eq!(
            low("COUNT(DISTINCT q0_0.\"id\")", SqlDialect::Postgres).unwrap(),
            ExprNode::Aggregate {
                func: AggregateFunc::Count,
                distinct: true,
                operand: b(col("id")),
                result: None,
            }
        );
        // MySQL's `SIGNED` cast collapses every integer width; it inverts to the canonical `I64` (which
        // re-renders to `SIGNED`, preserving identity) — a narrower original width is not recoverable.
        assert_eq!(
            low("CAST(SUM(q0_0.`amount`) AS SIGNED)", SqlDialect::Mysql).unwrap(),
            ExprNode::Aggregate {
                func: AggregateFunc::Sum,
                distinct: false,
                operand: b(col("amount")),
                result: Some(SqlType::I64),
            }
        );
    }

    #[test]
    fn now_lowers_only_the_exact_dialect_form() {
        assert_eq!(
            low("CURRENT_TIMESTAMP", SqlDialect::Postgres).unwrap(),
            ExprNode::Now
        );
        // MySQL's fractional `CURRENT_TIMESTAMP(6)` — the digit count is re-derived on render.
        assert_eq!(
            low("CURRENT_TIMESTAMP(6)", SqlDialect::Mysql).unwrap(),
            ExprNode::Now
        );
        // A precision the dialect's `now()` never emits must NOT lower to `Now` — re-rendering would emit
        // the dialect default and silently change the fractional-seconds precision. So: an explicit
        // precision on PostgreSQL, a non-`6` precision on MySQL, and a bare call read as MySQL all reject.
        assert!(matches!(
            low("CURRENT_TIMESTAMP(3)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low("CURRENT_TIMESTAMP(0)", SqlDialect::Postgres),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low("CURRENT_TIMESTAMP(3)", SqlDialect::Mysql),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low("CURRENT_TIMESTAMP", SqlDialect::Mysql),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn mysql_text_and_binary_result_pins_invert_to_canonical_types() {
        // MySQL renders a text-valued all-literal `COALESCE`/`NULLIF`/`CASE` result-pin as `CAST(… AS
        // CHAR)` (and a binary one as `CAST(… AS BINARY)`); both must invert so `read_create_view` can
        // read SQL squealy itself rendered. The keyword is many-to-one, so the inverse is the canonical
        // representative (`Text`/`Bytes`), which re-renders to the same keyword.
        assert_eq!(
            low(
                "COALESCE(CAST('x' AS CHAR), CAST('y' AS CHAR))",
                SqlDialect::Mysql
            )
            .unwrap(),
            ExprNode::Coalesce {
                args: vec![lit("'x'"), lit("'y'")],
                result: Some(SqlType::Text),
            }
        );
        assert_eq!(
            low("CAST(SUM(q0_0.`n`) AS SIGNED)", SqlDialect::Mysql).unwrap(),
            ExprNode::Aggregate {
                func: AggregateFunc::Sum,
                distinct: false,
                operand: b(col("n")),
                result: Some(SqlType::I64),
            }
        );
    }

    #[test]
    fn decimal_result_pins_invert_to_a_canonical_decimal() {
        // MySQL renders a `Decimal` pin as bare `DECIMAL` and SQLite as the `NUMERIC` affinity — both drop
        // the precision/scale, so a canonical `Decimal` re-renders to the same keyword (the invariant
        // holds; the exact precision is the backend PR's `canonical_view_*` job). Must invert, not reject.
        let decimal = |dialect, ty: &str| match low(
            &format!("CAST(SUM(q0_0.`n`) AS {ty})"),
            dialect,
        )
        .unwrap()
        {
            ExprNode::Aggregate { result, .. } => result,
            other => panic!("expected an aggregate, got: {other:?}"),
        };
        assert_eq!(
            decimal(SqlDialect::Mysql, "DECIMAL"),
            Some(SqlType::Decimal {
                precision: 10,
                scale: 0,
            })
        );
        // `DECIMAL(65, 0)` is instead the 128-bit-int pin.
        assert_eq!(
            decimal(SqlDialect::Mysql, "DECIMAL(65, 0)"),
            Some(SqlType::I128)
        );
        // SQLite uses double-quoted idents, so re-issue against a SQLite-quoted operand.
        assert_eq!(
            match low("CAST(SUM(q0_0.\"n\") AS NUMERIC)", SqlDialect::Sqlite).unwrap() {
                ExprNode::Aggregate { result, .. } => result,
                other => panic!("expected an aggregate, got: {other:?}"),
            },
            Some(SqlType::Decimal {
                precision: 10,
                scale: 0,
            })
        );
    }

    #[test]
    fn extract_recovers_field_pin_and_timezone() {
        // `EXTRACT(YEAR …)` with an `integer` pin (exact on PostgreSQL).
        assert_eq!(
            low(
                "CAST(EXTRACT(YEAR FROM q0_0.\"created\") AS integer)",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Extract {
                field: DateField::Year,
                operand: b(col("created")),
                result: Some(SqlType::I32),
                timezone: None,
            }
        );
        // The `AT TIME ZONE '<tz>'` operand wrapper is peeled into `timezone`.
        assert_eq!(
            low(
                "EXTRACT(HOUR FROM (q0_0.\"created\" AT TIME ZONE 'UTC'))",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Extract {
                field: DateField::Hour,
                operand: b(col("created")),
                result: None,
                timezone: Some("UTC".to_owned()),
            }
        );
        // `SECOND` is the fractional-seconds node (bare `EXTRACT(SECOND …)`), while
        // `FLOOR(EXTRACT(SECOND …))` is the whole-seconds `Extract`.
        assert_eq!(
            low(
                "EXTRACT(SECOND FROM q0_0.\"created\")",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::ExtractSecond {
                operand: b(col("created")),
                result: None,
            }
        );
        assert_eq!(
            low(
                "FLOOR(EXTRACT(SECOND FROM q0_0.\"created\"))",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Extract {
                field: DateField::Second,
                operand: b(col("created")),
                result: None,
                timezone: None,
            }
        );
    }

    #[test]
    fn date_trunc_lowers_unit_and_timezone() {
        assert_eq!(
            low(
                "date_trunc('month', q0_0.\"created\")",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::DateTrunc {
                unit: DateField::Month,
                operand: b(col("created")),
                timezone: None,
            }
        );
        assert_eq!(
            low(
                "date_trunc('day', q0_0.\"created\", 'UTC')",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::DateTrunc {
                unit: DateField::Day,
                operand: b(col("created")),
                timezone: Some("UTC".to_owned()),
            }
        );
        // `pg_get_viewdef` deparses the unit (and tz) string with a redundant `::text` cast; peel it.
        assert_eq!(
            low(
                "date_trunc('day'::text, q0_0.\"created\", 'UTC'::text)",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::DateTrunc {
                unit: DateField::Day,
                operand: b(col("created")),
                timezone: Some("UTC".to_owned()),
            }
        );
    }

    #[test]
    fn case_coalesce_nullif_recover_per_branch_casts() {
        // A searched `CASE` with bare (un-cast) branch values → `result: None`.
        assert_eq!(
            low(
                "CASE WHEN (q0_0.\"cnt\" > 10) THEN 'hi' ELSE 'lo' END",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Case {
                arms: vec![CaseArm {
                    when: b(ExprNode::Compare {
                        op: CompareOp::GreaterThan,
                        left: b(col("cnt")),
                        right: b(lit("10")),
                    }),
                    then: b(lit("'hi'")),
                }],
                else_: Some(b(lit("'lo'"))),
                result: None,
            }
        );
        // A `CASE` whose every branch value is `CAST(<v> AS bigint)` peels back to `result: Some(I64)`.
        assert_eq!(
            low(
                "CASE WHEN (q0_0.\"cnt\" > 10) THEN CAST(1 AS bigint) ELSE CAST(0 AS bigint) END",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Case {
                arms: vec![CaseArm {
                    when: b(ExprNode::Compare {
                        op: CompareOp::GreaterThan,
                        left: b(col("cnt")),
                        right: b(lit("10")),
                    }),
                    then: b(lit("1")),
                }],
                else_: Some(b(lit("0"))),
                result: Some(SqlType::I64),
            }
        );
        // `COALESCE` with a column argument (not all literals) → no per-branch cast → `result: None`.
        assert_eq!(
            low("COALESCE(q0_0.\"amount\", 0)", SqlDialect::Postgres).unwrap(),
            ExprNode::Coalesce {
                args: vec![col("amount"), lit("0")],
                result: None,
            }
        );
        // An all-literal `COALESCE` casts every argument → `result: Some(I64)`, bare literals recovered.
        assert_eq!(
            low(
                "COALESCE(CAST(1 AS bigint), CAST(0 AS bigint))",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Coalesce {
                args: vec![lit("1"), lit("0")],
                result: Some(SqlType::I64),
            }
        );
        // `NULLIF` with a column operand → `result: None`.
        assert_eq!(
            low("NULLIF(q0_0.\"cnt\", 0)", SqlDialect::Postgres).unwrap(),
            ExprNode::Nullif {
                left: b(col("cnt")),
                right: b(lit("0")),
                result: None,
            }
        );
        // A simple `CASE <operand> WHEN …` lowers to `SimpleCase`.
        assert!(matches!(
            low(
                "CASE q0_0.\"cnt\" WHEN 1 THEN 'a' ELSE 'b' END",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::SimpleCase { .. }
        ));
        // A branch-cast mix (one branch a *function-style* cast, one not) is outside the emitted grammar.
        assert!(matches!(
            low(
                "COALESCE(CAST(1 AS bigint), q0_0.\"amount\")",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        // But `pg_get_viewdef` wraps a bare LITERAL branch in a redundant `::type` cast even in an
        // un-pinned expression (`COALESCE(amount, (0)::bigint)`); that is deparse noise, not a mixed pin,
        // so it lowers to the bare literal with `result: None` (matching the renderer's `COALESCE(amount, 0)`).
        assert_eq!(
            low(
                "COALESCE(q0_0.\"amount\", (0)::bigint)",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Coalesce {
                args: vec![col("amount"), lit("0")],
                result: None,
            }
        );
        // The deparsed `::` form of a genuine all-branch pin still recovers `result` (round-2 case).
        assert_eq!(
            low(
                "CASE WHEN (q0_0.\"cnt\" > 10) THEN (q0_0.\"cnt\")::bigint ELSE (0)::bigint END",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Case {
                arms: vec![CaseArm {
                    when: b(ExprNode::Compare {
                        op: CompareOp::GreaterThan,
                        left: b(col("cnt")),
                        right: b(lit("10")),
                    }),
                    then: b(col("cnt")),
                }],
                else_: Some(b(lit("0"))),
                result: Some(SqlType::I64),
            }
        );
    }

    #[test]
    fn simple_window_lowers_partition_and_order() {
        assert_eq!(
            low(
                "ROW_NUMBER() OVER (PARTITION BY q0_0.\"name\" ORDER BY q0_0.\"id\" ASC)",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Window {
                func: WindowFunc::RowNumber,
                args: Vec::new(),
                partition_by: vec![col("name")],
                order_by: vec![WindowOrderTerm {
                    expr: col("id"),
                    direction: OrderDirection::Asc,
                }],
                frame: None,
                result: None,
            }
        );
        // An aggregate used as a window, with a `bigint` result-pin.
        assert_eq!(
            low(
                "CAST(SUM(q0_0.\"amount\") OVER (PARTITION BY q0_0.\"name\") AS bigint)",
                SqlDialect::Postgres
            )
            .unwrap(),
            ExprNode::Window {
                func: WindowFunc::Aggregate(AggregateFunc::Sum),
                args: vec![col("amount")],
                partition_by: vec![col("name")],
                order_by: Vec::new(),
                frame: None,
                result: Some(SqlType::I64),
            }
        );
    }

    // ---- single-SELECT view-body lowering -----------------------------------------------------

    #[test]
    fn create_view_with_column_list_names_projections_positionally() {
        let query = low_query(
            "CREATE VIEW \"public\".\"v\" (\"added\", \"id\") AS \
             SELECT (q0_0.\"cnt\" + 1), q0_0.\"id\" FROM \"public\".\"events\" AS q0_0 \
             WHERE (q0_0.\"cnt\" > 0) GROUP BY q0_0.\"name\" ORDER BY q0_0.\"id\" DESC LIMIT 10 OFFSET 5",
            SqlDialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            query,
            ViewQueryModel {
                distinct: false,
                projection: vec![
                    ProjectionItem {
                        output_name: "added".to_owned(),
                        expr: ExprNode::Binary {
                            op: ArithmeticOp::Add,
                            left: b(col("cnt")),
                            right: b(lit("1")),
                        },
                    },
                    ProjectionItem {
                        output_name: "id".to_owned(),
                        expr: col("id"),
                    },
                ],
                from: Some(SourceItem::Named(SourceRef {
                    schema: Some("public".to_owned()),
                    name: "events".to_owned(),
                    alias: "q0_0".to_owned(),
                })),
                joins: Vec::new(),
                filter: Some(ExprNode::Compare {
                    op: CompareOp::GreaterThan,
                    left: b(col("cnt")),
                    right: b(lit("0")),
                }),
                group_by: vec![col("name")],
                having: None,
                order_by: vec![OrderItem {
                    expr: col("id"),
                    direction: Some(OrderDirection::Desc),
                    nulls: None,
                }],
                limit: Some(10),
                offset: Some(5),
                dependencies: Vec::new(),
            }
        );
    }

    #[test]
    fn column_list_names_win_over_an_inner_projection_alias() {
        // When a `CREATE VIEW (cols)` list is present, SQL names the outputs from the list — even if a
        // projection also carries its own `AS` alias, the declared column name (`out`) wins over it
        // (`inner`). (squealy never emits this combination, but external / hand-authored SQL can.)
        let query = low_query(
            "CREATE VIEW \"v\" (\"out\") AS SELECT 1 AS \"inner\"",
            SqlDialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            query.projection,
            vec![ProjectionItem {
                output_name: "out".to_owned(),
                expr: lit("1"),
            }],
        );
    }

    #[test]
    fn distinct_and_join_and_aliased_projection_lower() {
        // A bare `SELECT` (as a view-body deparse returns) names its projections by their `AS` aliases.
        let query = low_query(
            "SELECT DISTINCT q0_0.\"id\" AS id FROM \"public\".\"events\" AS q0_0 \
             LEFT JOIN \"public\".\"other\" AS q0_1 ON (q0_0.\"id\" = q0_1.\"id\")",
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(query.distinct);
        assert_eq!(query.projection.len(), 1);
        assert_eq!(query.projection[0].output_name, "id");
        assert_eq!(query.joins.len(), 1);
        assert_eq!(query.joins[0].kind, JoinKind::Left);
        assert_eq!(
            query.joins[0].source,
            SourceItem::Named(SourceRef {
                schema: Some("public".to_owned()),
                name: "other".to_owned(),
                alias: "q0_1".to_owned(),
            })
        );
        assert!(query.joins[0].on.is_some());
    }

    #[test]
    fn mysql_offset_only_sentinel_limit_recovers_to_none() {
        // MySQL has no bare `OFFSET`, so the renderer emits an offset-only view as
        // `LIMIT <u64::MAX> OFFSET n`. The max-u64 limit is the "all rows" sentinel, not a real bound, so
        // it must recover to `limit: None` (else the model carries `Some(u64::MAX)` and churns).
        let query = low_query(
            "SELECT q0_0.`id` AS id FROM `events` AS q0_0 LIMIT 18446744073709551615 OFFSET 5",
            SqlDialect::Mysql,
        )
        .unwrap();
        assert_eq!(query.limit, None);
        assert_eq!(query.offset, Some(5));
        // A genuine limit is unaffected.
        let bounded = low_query(
            "SELECT q0_0.`id` AS id FROM `events` AS q0_0 LIMIT 10 OFFSET 5",
            SqlDialect::Mysql,
        )
        .unwrap();
        assert_eq!(bounded.limit, Some(10));
        assert_eq!(bounded.offset, Some(5));
        // A *bare* `LIMIT <u64::MAX>` (no OFFSET) is the renderer's output for a genuine `Some(usize::MAX)`
        // limit, not the offset-only sentinel — it must round-trip unchanged, not collapse to `None`.
        let bare_max = low_query(
            "SELECT q0_0.`id` AS id FROM `events` AS q0_0 LIMIT 18446744073709551615",
            SqlDialect::Mysql,
        )
        .unwrap();
        assert_eq!(bare_max.limit, Some(18446744073709551615));
        assert_eq!(bare_max.offset, None);
    }

    #[test]
    fn cross_join_has_no_on_condition() {
        let query = low_query(
            "SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0 \
             CROSS JOIN \"public\".\"b\" AS q0_1",
            SqlDialect::Postgres,
        )
        .unwrap();
        assert_eq!(query.joins.len(), 1);
        assert_eq!(query.joins[0].kind, JoinKind::Cross);
        assert!(query.joins[0].on.is_none());
    }

    #[test]
    fn derived_table_from_source_lowers_to_a_subquery() {
        // `FROM (<subquery>) AS q0_0` — the derived body lowers like any other view query and the alias
        // binds it. The outer projection references the derived alias.
        let query = low_query(
            "SELECT q0_0.\"id\" AS id \
             FROM (SELECT q0_1.\"id\" AS id FROM \"public\".\"events\" AS q0_1 WHERE (q0_1.\"cnt\" > 0)) \
             AS q0_0",
            SqlDialect::Postgres,
        )
        .unwrap();
        let SourceItem::Derived {
            query: inner,
            alias,
        } = query.from.expect("a FROM source")
        else {
            panic!("expected a derived FROM source");
        };
        assert_eq!(alias, "q0_0");
        let inner = select_of(&inner);
        assert_eq!(inner.projection.len(), 1);
        assert_eq!(inner.projection[0].output_name, "id");
        assert_eq!(
            inner.from,
            Some(SourceItem::Named(SourceRef {
                schema: Some("public".to_owned()),
                name: "events".to_owned(),
                alias: "q0_1".to_owned(),
            }))
        );
        assert!(inner.filter.is_some());
    }

    #[test]
    fn derived_table_in_a_join_lowers() {
        // A derived table on the right of a `JOIN … ON`.
        let query = low_query(
            "SELECT q0_0.\"id\" AS id FROM \"public\".\"events\" AS q0_0 \
             INNER JOIN (SELECT q0_1.\"id\" AS id FROM \"public\".\"other\" AS q0_1) AS q0_2 \
             ON (q0_0.\"id\" = q0_2.\"id\")",
            SqlDialect::Postgres,
        )
        .unwrap();
        assert_eq!(query.joins.len(), 1);
        assert_eq!(query.joins[0].kind, JoinKind::Inner);
        let SourceItem::Derived { alias, .. } = &query.joins[0].source else {
            panic!("expected a derived join source");
        };
        assert_eq!(alias, "q0_2");
        assert!(query.joins[0].on.is_some());
    }

    #[test]
    fn a_lateral_derived_table_is_not_yet_lowered() {
        // `LATERAL` carries correlation semantics the neutral model cannot hold. (A *set-op* derived
        // table body now lowers — see `set_op_view_bodies_lower`.)
        assert!(matches!(
            low_query(
                "SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_1 \
                 CROSS JOIN LATERAL (SELECT q0_1.\"id\" AS id) AS q0_0",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn view_body_shapes_outside_the_grammar_are_not_yet_lowered() {
        // (A `WITH` prelude now lowers to `ViewBody::With` — see `a_with_clause_lowers_to_a_with_body`.)
        // Comma-separated FROM (implicit cross join).
        assert!(matches!(
            low_query(
                "SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0, \"public\".\"b\" AS q0_1",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        // A wildcard projection cannot be named.
        assert!(matches!(
            low_query(
                "SELECT * FROM \"public\".\"a\" AS q0_0",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        // A `CREATE VIEW` modifier the renderer never emits (here `MATERIALIZED`) is rejected, not lowered
        // as an ordinary view (which would drop the materialized semantics on re-render).
        assert!(matches!(
            low_query(
                "CREATE MATERIALIZED VIEW v AS SELECT 1 AS n",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        // `OR REPLACE` is accepted — the renderer emits it and it does not change the body.
        assert!(
            low_query(
                "CREATE OR REPLACE VIEW v AS SELECT 1 AS n",
                SqlDialect::Postgres
            )
            .is_ok()
        );
        // A query-level clause the model cannot hold is rejected, not silently dropped: `FETCH` and
        // `FOR UPDATE` (both attach to the `Query`, which this path otherwise only reads ORDER/LIMIT from).
        assert!(matches!(
            low_query(
                "SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0 FETCH FIRST 1 ROW ONLY",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low_query(
                "SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0 FOR UPDATE",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        // A source alias the renderer could not re-emit unquoted is rejected (it emits `AS {alias}` bare):
        // a special-character alias, a mixed-case alias (unquoted parses case-folded), and a reserved word.
        assert!(matches!(
            low_query(
                "SELECT \"q-1\".\"id\" AS id FROM \"public\".\"a\" AS \"q-1\"",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low_query(
                "SELECT \"Q0\".\"id\" AS id FROM \"public\".\"a\" AS \"Q0\"",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
        assert!(matches!(
            low_query(
                "SELECT \"select\".\"id\" AS id FROM \"public\".\"a\" AS \"select\"",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn set_op_view_bodies_lower() {
        // A plain `UNION` over two parenthesized (`SetExpr::Query`) operands.
        let body = low_body(
            "(SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0) \
             UNION (SELECT q0_1.\"id\" AS id FROM \"public\".\"b\" AS q0_1)",
            SqlDialect::Postgres,
        )
        .unwrap();
        let ViewBody::Set {
            op,
            all,
            left,
            right,
            order_by,
            limit,
            offset,
        } = &body
        else {
            panic!("expected a set body, got {body:?}");
        };
        assert_eq!(*op, ViewSetOp::Union);
        assert!(!*all);
        assert!(order_by.is_empty() && limit.is_none() && offset.is_none());
        // Each operand is a single SELECT naming the right source.
        assert_eq!(select_of(left).from.as_ref().unwrap().alias(), "q0_0");
        assert_eq!(select_of(right).from.as_ref().unwrap().alias(), "q0_1");

        // `INTERSECT ALL` maps to `op = Intersect, all = true`. Bare (unparenthesized) operands lower too.
        let body = low_body(
            "SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0 \
             INTERSECT ALL SELECT q0_1.\"id\" AS id FROM \"public\".\"b\" AS q0_1",
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(matches!(
            body,
            ViewBody::Set {
                op: ViewSetOp::Intersect,
                all: true,
                ..
            }
        ));

        // A trailing whole-set `ORDER BY`/`LIMIT` attaches to the `Set`, not the arms.
        let body = low_body(
            "SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0 \
             UNION SELECT q0_1.\"id\" AS id FROM \"public\".\"b\" AS q0_1 \
             ORDER BY id LIMIT 5",
            SqlDialect::Postgres,
        )
        .unwrap();
        let ViewBody::Set {
            order_by,
            limit,
            left,
            ..
        } = &body
        else {
            panic!("expected a set body");
        };
        assert_eq!(order_by.len(), 1);
        assert_eq!(*limit, Some(5));
        // The arm keeps no ORDER BY/LIMIT of its own.
        assert!(select_of(left).order_by.is_empty() && select_of(left).limit.is_none());
    }

    #[test]
    fn a_parenthesized_body_keeps_its_outer_order_and_limit() {
        // sqlparser attaches a trailing `ORDER BY`/`LIMIT` on a parenthesized body to the *outer* query
        // (`body: SetExpr::Query`). It must not be dropped (which would re-render a different result set).
        // A parenthesized single SELECT: the tail folds onto the leaf.
        let body = low_body(
            "(SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0) ORDER BY id LIMIT 5",
            SqlDialect::Postgres,
        )
        .unwrap();
        let select = select_of(&body);
        assert_eq!(select.order_by.len(), 1);
        assert_eq!(select.limit, Some(5));

        // A parenthesized set operation: the outer tail folds onto the `Set` node.
        let body = low_body(
            "(SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0 \
             UNION SELECT q0_1.\"id\" AS id FROM \"public\".\"b\" AS q0_1) ORDER BY id LIMIT 5",
            SqlDialect::Postgres,
        )
        .unwrap();
        let ViewBody::Set {
            order_by, limit, ..
        } = &body
        else {
            panic!("expected a set body");
        };
        assert_eq!(order_by.len(), 1);
        assert_eq!(*limit, Some(5));

        // Two nested tails (inner + outer) cannot be represented — rejected, not silently collapsed.
        assert!(matches!(
            low_body(
                "(SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0 ORDER BY id LIMIT 3) \
                 ORDER BY id LIMIT 5",
                SqlDialect::Postgres,
            ),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn sqlite_select_star_set_operands_peel() {
        // SQLite renders each operand as `SELECT * FROM (<operand>)`; the reverse path peels the wrapper
        // back to the inner operand so the body lowers to the same `Set` as the parenthesized form.
        let body = low_body(
            "SELECT * FROM (SELECT q0_0.\"id\" AS id FROM \"a\" AS q0_0) \
             UNION SELECT * FROM (SELECT q0_1.\"id\" AS id FROM \"b\" AS q0_1)",
            SqlDialect::Sqlite,
        )
        .unwrap();
        let ViewBody::Set { left, right, .. } = &body else {
            panic!("expected a set body, got {body:?}");
        };
        // Each operand peeled to its inner single SELECT (not a `SELECT *` over a derived table).
        assert_eq!(select_of(left).projection[0].output_name, "id");
        assert_eq!(select_of(left).from.as_ref().unwrap().alias(), "q0_0");
        assert_eq!(select_of(right).from.as_ref().unwrap().alias(), "q0_1");
        // A genuine `SELECT *` (over a named table, not the wrapper) is still not lowerable.
        assert!(matches!(
            low_body("SELECT * FROM \"a\" AS q0_0", SqlDialect::Sqlite),
            Err(ReadError::NotYetLowered(_))
        ));
        // The wrapper shape carrying another select-level clause (`QUALIFY`) must NOT peel-and-drop that
        // clause — it surfaces as NotYetLowered (the outer select is rejected before the peel).
        assert!(matches!(
            low_body(
                "SELECT * FROM (SELECT q0_0.\"id\" AS id FROM \"a\" AS q0_0) \
                 QUALIFY ROW_NUMBER() OVER () = 1 \
                 UNION SELECT * FROM (SELECT q0_1.\"id\" AS id FROM \"b\" AS q0_1)",
                SqlDialect::Sqlite,
            ),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn set_op_in_a_scalar_subquery_is_not_yet_lowered() {
        // The expression-subquery IR node is single-SELECT only, so a set-op subquery there is rejected.
        assert!(matches!(
            low_query(
                "SELECT q0_0.\"id\" AS id FROM \"public\".\"a\" AS q0_0 \
                 WHERE q0_0.\"id\" IN (SELECT q1_0.\"id\" FROM \"public\".\"b\" AS q1_0 \
                 UNION SELECT q1_1.\"id\" FROM \"public\".\"c\" AS q1_1)",
                SqlDialect::Postgres
            ),
            Err(ReadError::NotYetLowered(_))
        ));
    }

    #[test]
    fn a_with_clause_lowers_to_a_with_body() {
        // A non-recursive `WITH` — a column-listed CTE plus a main body selecting from it. The prelude
        // becomes a `ViewBody::With`; its `recursive` flag is false, the CTE's declared columns are
        // captured, and the main body wraps unchanged.
        let body = low_body(
            "WITH \"recent\" (\"id\") AS (SELECT q1_0.\"id\" AS id FROM \"public\".\"events\" AS q1_0) \
             SELECT q0_0.\"id\" AS id FROM \"recent\" AS q0_0",
            SqlDialect::Postgres,
        )
        .unwrap();
        let ViewBody::With {
            recursive,
            ctes,
            body,
        } = &body
        else {
            panic!("expected a WITH body, got {body:?}");
        };
        assert!(!*recursive);
        assert_eq!(ctes.len(), 1);
        assert_eq!(ctes[0].name, "recent");
        assert_eq!(ctes[0].columns, vec!["id".to_owned()]);
        // The CTE body reads the real table; the main body reads the bound CTE name (schema-less).
        assert_eq!(
            select_of(&ctes[0].body).from.as_ref().unwrap().alias(),
            "q1_0"
        );
        let SourceItem::Named(main_from) = select_of(body).from.as_ref().unwrap() else {
            panic!("expected a named main-body source");
        };
        assert_eq!(main_from.name, "recent");
        assert_eq!(main_from.schema, None);
    }

    #[test]
    fn a_with_recursive_clause_lowers_to_a_recursive_with_body() {
        // A `WITH RECURSIVE` counter written the canonical way — bare `UNION ALL` arms with an **un-aliased
        // anchor** (`SELECT 1`), as a human or an introspected database emits it. The CTE column list
        // (`n`) names each arm's otherwise-unnamed projection as a *fallback* (arms keep any own aliases),
        // so both the un-aliased anchor and the `n + 1` recursive projection lower to output column `n`.
        // The body is a `UNION ALL` set (no special recursive variant); the clause-level `recursive` is set.
        let body = low_body(
            "WITH RECURSIVE \"counter\" (\"n\") AS \
             (SELECT 1 UNION ALL \
              SELECT q0_0.\"n\" + 1 FROM \"counter\" AS q0_0 WHERE q0_0.\"n\" < 10) \
             SELECT q0_0.\"n\" AS n FROM \"counter\" AS q0_0",
            SqlDialect::Postgres,
        )
        .unwrap();
        let ViewBody::With {
            recursive, ctes, ..
        } = &body
        else {
            panic!("expected a WITH body, got {body:?}");
        };
        assert!(*recursive);
        assert_eq!(ctes[0].name, "counter");
        let ViewBody::Set {
            op,
            all,
            left,
            right,
            ..
        } = &ctes[0].body
        else {
            panic!("expected the recursive CTE body to be a set");
        };
        assert_eq!(*op, ViewSetOp::Union);
        assert!(*all);
        // The un-aliased anchor `SELECT 1` is named `n` by the CTE column list (fallback).
        assert_eq!(select_of(left).projection[0].output_name, "n");
        assert!(select_of(left).from.is_none());
        // The recursive arm's un-aliased `n + 1` projection is likewise named `n`, and its `FROM`
        // references the CTE by its bound name.
        assert_eq!(select_of(right).projection[0].output_name, "n");
        assert_eq!(select_of(right).from.as_ref().unwrap().alias(), "q0_0");
    }

    #[test]
    fn a_column_listed_set_arm_keeps_its_own_alias_over_the_list() {
        // For a set body the column list (`n`) is only a *fallback*: an arm projection that carries its own
        // alias keeps it (so a per-arm `ORDER BY <alias>` still resolves), and only an un-aliased projection
        // is named by the list. Here the left arm aliases `total` (kept) while the right arm is un-aliased
        // (named `n`). A `CREATE VIEW (n) AS …` supplies the column list.
        let body = low_body(
            "CREATE VIEW v (n) AS \
             SELECT q0_0.\"amount\" * 2 AS total FROM \"public\".\"a\" AS q0_0 \
             UNION ALL SELECT q0_1.\"amount\" + 2 FROM \"public\".\"b\" AS q0_1",
            SqlDialect::Postgres,
        )
        .unwrap();
        let ViewBody::Set { left, right, .. } = &body else {
            panic!("expected a set body, got {body:?}");
        };
        // Left arm keeps its own alias; right arm (un-aliased) is named by the fallback column list.
        assert_eq!(select_of(left).projection[0].output_name, "total");
        assert_eq!(select_of(right).projection[0].output_name, "n");
    }

    #[test]
    fn a_cte_column_list_arity_mismatch_is_rejected() {
        // A CTE column list whose arity differs from the body's projection is rejected (`sqlparser` does
        // not validate this), not silently mis-indexed — the shared projection-arity check in
        // `lower_select` fires for both a single-`SELECT` and a set body.
        assert!(matches!(
            low_body(
                "WITH \"c\" (\"a\") AS (SELECT 1, 2) SELECT q0_0.\"a\" AS a FROM \"c\" AS q0_0",
                SqlDialect::Postgres,
            ),
            Err(ReadError::Unexpected(_))
        ));
        assert!(matches!(
            low_body(
                "WITH \"c\" (\"a\") AS (SELECT 1, 2 UNION ALL SELECT 3, 4) \
                 SELECT q0_0.\"a\" AS a FROM \"c\" AS q0_0",
                SqlDialect::Postgres,
            ),
            Err(ReadError::Unexpected(_))
        ));
    }

    #[test]
    fn a_materialized_cte_is_not_yet_lowered() {
        // `AS MATERIALIZED` / `AS NOT MATERIALIZED` carries planner semantics the neutral model does not
        // hold — rejected rather than silently dropped.
        assert!(matches!(
            low_body(
                "WITH \"m\" AS MATERIALIZED (SELECT q1_0.\"id\" AS id FROM \"public\".\"a\" AS q1_0) \
                 SELECT q0_0.\"id\" AS id FROM \"m\" AS q0_0",
                SqlDialect::Postgres,
            ),
            Err(ReadError::NotYetLowered(_))
        ));
    }
}
