//! Dialect-driven rendering of view DDL from the neutral [`ViewModel`].
//!
//! View bodies are stored structurally ([`ViewQueryModel`]/[`ExprNode`]); this module renders them to
//! SQL for a given [`Dialect`], so each backend gets dialect-correct identifier quoting, cast type
//! names, integer-division casts, and `LIKE`/`ILIKE` from one shared renderer. Backends call
//! [`render_create_view`]/[`render_drop_view`] (and [`ordered_views`] for create-from-scratch order).

use std::io::{self, Write};

use crate::{
    AggregateFunc, ArithmeticOp, CteModel, DatabaseModel, DateField, Dialect, ExprNode, JoinKind,
    LogicalOp, OrderDirection, OrderItem, ScalarFunc, SetOperandStyle, SourceItem, SqlType,
    UnaryStringFunc, ViewBody, ViewModel, ViewQueryModel, ViewSetOp, WindowFunc,
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
    if view.query.is_empty() {
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
    render_body(&view.query, view.columns.is_empty(), dialect, writer)
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
    // A backend without namespaces (SQLite) suppresses the schema qualifier — a qualified name there is
    // read as `"attached_db"."table"`, not `"schema"."table"`. The query-side `write_table_ref` honors
    // the same seam, so view sources and query sources render alike.
    if dialect.qualify_schema()
        && let Some(schema) = schema
    {
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
        } else if let Some(internal_alias) = &item.internal_alias {
            // A `CREATE VIEW (<cols>)` list renames this projection's output and suppresses its own `AS`,
            // but the body's own `ORDER BY`/`GROUP BY`/`HAVING` reference the inner alias — re-emit it so
            // those clauses still resolve (else the reference dangles and the DDL is invalid).
            writer.write_all(b" AS ")?;
            dialect.write_quoted_ident(internal_alias, writer)?;
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
            JoinKind::Cross => b" CROSS JOIN ".as_slice(),
        })?;
        render_source(&join.source, dialect, writer)?;
        // `CROSS JOIN` has no condition; the others carry an `ON`.
        if let Some(on) = &join.on {
            writer.write_all(b" ON ")?;
            render_expr(on, dialect, writer)?;
        }
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

    render_order_limit(&query.order_by, query.limit, query.offset, dialect, writer)
}

/// Renders a trailing `ORDER BY … [ASC|DESC] [NULLS …]` then `LIMIT`/`OFFSET`. Shared by a single
/// `SELECT` body and the whole-set tail of a set operation.
fn render_order_limit(
    order_by: &[OrderItem],
    limit: Option<usize>,
    offset: Option<usize>,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    for (index, order) in order_by.iter().enumerate() {
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
    dialect.write_limit_offset(limit, offset, writer)
}

/// Renders a view body — a single `SELECT` or a set operation. `alias_projections` names the outputs by
/// aliasing each projected expression (used when the view carries no declared column list); for a set
/// body it applies to the **leftmost** `SELECT` only, since SQL takes a compound's output names from its
/// first arm.
fn render_body(
    body: &ViewBody,
    alias_projections: bool,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    match body {
        ViewBody::Select(query) => render_select(query, alias_projections, dialect, writer),
        ViewBody::Set {
            op,
            all,
            left,
            right,
            order_by,
            limit,
            offset,
        } => {
            // A plain set body wraps its operands per the dialect: PostgreSQL/MySQL with `(…)`; SQLite
            // rejects a parenthesized compound operand and wraps with `SELECT * FROM (…)`. This is the
            // same [`Dialect::set_operand_style`] seam the runtime set renderer honors, so a set-op *view
            // body* renders identically to a runtime set query — the reverse parser inverts both wrappings.
            let (open, close): (&[u8], &[u8]) = match dialect.set_operand_style() {
                SetOperandStyle::Parenthesized => (b"(", b")"),
                SetOperandStyle::SubquerySelect => (b"SELECT * FROM (", b")"),
            };
            // `alias_projections` is only consulted for a single-`SELECT` body; each set arm always
            // aliases its projections (see [`render_set`]).
            let _ = alias_projections;
            render_set(
                *op,
                *all,
                left,
                right,
                order_by,
                *limit,
                *offset,
                OperandWrap::Fixed { open, close },
                dialect,
                writer,
            )
        }
        ViewBody::With {
            recursive,
            ctes,
            body,
        } => {
            render_with_prefix(*recursive, ctes, dialect, writer)?;
            render_body(body, alias_projections, dialect, writer)
        }
    }
}

/// How a set operation wraps each of its operands (see [`render_set_operand`]).
enum OperandWrap<'a> {
    /// A plain compound: every operand is wrapped by the same fixed `open`/`close` delimiters (the
    /// dialect's [`SetOperandStyle`]).
    Fixed { open: &'a [u8], close: &'a [u8] },
    /// A recursive-CTE body's `<anchor> UNION [ALL] <recursive>`: each arm is wrapped **per arm** — a
    /// plain tail-less arm renders bare (required for SQLite, valid everywhere), a scoped arm is
    /// parenthesized where the dialect permits it and rejected where it does not
    /// ([`Dialect::supports_parenthesized_recursive_cte_arm`]).
    RecursiveArm,
}

/// Renders a set operation — `<left> <OP> <right>` — with each operand wrapped per `wrap`, followed by any
/// trailing whole-set `ORDER BY`/`LIMIT`/`OFFSET`. A plain set body passes [`OperandWrap::Fixed`] with the
/// dialect's [`SetOperandStyle`] delimiters; a recursive-CTE set body (rendered by [`render_with_prefix`])
/// passes [`OperandWrap::RecursiveArm`] for per-arm bare/parenthesized handling.
#[allow(clippy::too_many_arguments)]
fn render_set(
    op: ViewSetOp,
    all: bool,
    left: &ViewBody,
    right: &ViewBody,
    order_by: &[OrderItem],
    limit: Option<usize>,
    offset: Option<usize>,
    wrap: OperandWrap,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    // `INTERSECT ALL`/`EXCEPT ALL` are unsupported on some dialects (SQLite allows `ALL` only
    // after `UNION`); reject rather than emit SQL the target cannot run.
    if all
        && matches!(op, ViewSetOp::Intersect | ViewSetOp::Except)
        && !dialect.supports_intersect_except_all()
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "this dialect does not support `{}` in a view body",
                set_op_keyword(op, all)
            ),
        ));
    }
    // A compound (set-operation) `ORDER BY` resolves against the set's *output* columns — which
    // are named by the **leftmost arm's** projection aliases (not the `CREATE VIEW` column list,
    // and not an arm's `FROM` alias). So each whole-set order term must be either a bare name that
    // the leftmost arm actually emits, or an ordinal `1..=N` over those outputs. Anything else (an
    // alias-qualified column, an arbitrary expression, a name/ordinal that does not resolve — none
    // of which the lowering path produces for a set tail, but a hand-built / packaged model can)
    // would fail when the view is queried, so reject it here.
    let outputs = &leftmost_select(left).projection;
    for order in order_by {
        let resolves = match &order.expr {
            ExprNode::BareColumn { column } => {
                outputs.iter().any(|item| &item.output_name == column)
            }
            ExprNode::Literal(text) => {
                matches!(text.parse::<usize>(), Ok(n) if (1..=outputs.len()).contains(&n))
            }
            _ => false,
        };
        if !resolves {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a set-operation ORDER BY term must reference a leftmost-arm output column by \
                 name or a 1-based ordinal, not an alias-qualified column or expression",
            ));
        }
    }
    // Every arm always aliases its projections. The compound's output names still come from the leftmost
    // arm's aliases (or the column list, which overrides them on read-back — so this stays round-trip
    // stable), but each arm additionally *needs* its aliases for its own clauses: a per-arm `ORDER BY
    // <alias>` on either arm dangles without them, and an aliased *expression* projection has no other
    // name to lower back to.
    render_set_operand(left, &wrap, dialect, writer)?;
    write!(writer, " {}", set_op_keyword(op, all))?;
    writer.write_all(b" ")?;
    render_set_operand(right, &wrap, dialect, writer)?;
    // A trailing ORDER BY/LIMIT over the whole set (after the final arm).
    render_order_limit(order_by, limit, offset, dialect, writer)
}

/// The leftmost single-`SELECT` of a view body — a `Select` directly, or (recursively) the left arm of a
/// set operation / the inner body of a `With`. Its projection names the compound's output columns, which
/// a whole-set `ORDER BY` resolves against.
fn leftmost_select(body: &ViewBody) -> &ViewQueryModel {
    match body {
        ViewBody::Select(query) => query,
        ViewBody::Set { left, .. } => leftmost_select(left),
        ViewBody::With { body, .. } => leftmost_select(body),
    }
}

/// Renders one operand of a set operation. For a plain compound ([`OperandWrap::Fixed`]) the operand is
/// wrapped by fixed `open`/`close` delimiters so its own `ORDER BY`/`LIMIT` (and a nested compound's
/// grouping) binds to the operand and not the enclosing set. For a recursive-CTE body
/// ([`OperandWrap::RecursiveArm`]) each arm is wrapped per-arm (see [`render_recursive_arm`]).
fn render_set_operand(
    body: &ViewBody,
    wrap: &OperandWrap,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    match wrap {
        OperandWrap::Fixed { open, close } => {
            writer.write_all(open)?;
            render_body(body, true, dialect, writer)?;
            writer.write_all(close)
        }
        OperandWrap::RecursiveArm => render_recursive_arm(body, dialect, writer),
    }
}

/// Renders one arm of a recursive-CTE body. A plain, tail-less `SELECT` renders **bare** — a bare arm is
/// required by SQLite (which rejects any parenthesized recursive arm) and is valid on every dialect. An arm
/// that carries its own `ORDER BY`/`LIMIT`/`OFFSET`, or is a nested compound, can only be scoped by
/// parenthesizing it: rendered `(<arm>)` where the dialect permits a parenthesized recursive arm
/// ([`Dialect::supports_parenthesized_recursive_cte_arm`]), and rejected where it does not (SQLite — such an
/// arm has no valid rendering there). Arms are aliased like any set operand.
fn render_recursive_arm(
    arm: &ViewBody,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    let is_plain_tailless = matches!(
        arm,
        ViewBody::Select(select)
            if select.order_by.is_empty() && select.limit.is_none() && select.offset.is_none()
    );
    if is_plain_tailless {
        render_body(arm, true, dialect, writer)
    } else if dialect.supports_parenthesized_recursive_cte_arm() {
        writer.write_all(b"(")?;
        render_body(arm, true, dialect, writer)?;
        writer.write_all(b")")
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this dialect cannot render a recursive CTE arm that carries its own ORDER BY/LIMIT/OFFSET \
             or is a nested set operation — its recursive-CTE grammar forbids parenthesizing an arm, so \
             such an arm cannot be operand-scoped",
        ))
    }
}

/// Renders a `WITH [RECURSIVE] <name> [(<cols>)] AS (<body>)[, …] ` prelude (note the trailing space) for a
/// [`ViewBody::With`]. Each CTE body renders via [`render_body`], except a **recursive** CTE — one whose
/// set body references its own name — whose `<anchor> UNION [ALL] <recursive>` arms render per-arm (bare, or
/// parenthesized where the dialect permits a scoped arm; see [`render_recursive_arm`]).
///
/// This is the single shared `WITH`-prefix renderer: both a view body's `WITH` prelude and the runtime
/// query path (`render.rs::write_cte_prefix`, which builds a `&[CteModel]` from the collected `CteDef`s)
/// route through it.
pub fn render_with_prefix(
    recursive: bool,
    ctes: &[CteModel],
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    if ctes.is_empty() {
        return Ok(());
    }
    // SQL requires `WITH RECURSIVE` on the whole clause if any CTE is recursive.
    writer.write_all(if recursive {
        b"WITH RECURSIVE ".as_slice()
    } else {
        b"WITH ".as_slice()
    })?;
    for (index, cte) in ctes.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        dialect.write_quoted_ident(&cte.name, writer)?;
        // The optional `WITH` column list. Absent when the CTE declares no columns — its body's own
        // projection names the outputs — so an empty `()` is never emitted.
        if !cte.columns.is_empty() {
            writer.write_all(b" (")?;
            for (column_index, column) in cte.columns.iter().enumerate() {
                if column_index > 0 {
                    writer.write_all(b", ")?;
                }
                dialect.write_quoted_ident(column, writer)?;
            }
            writer.write_all(b")")?;
        }
        writer.write_all(b" AS (")?;
        render_cte_definition_body(cte, recursive, dialect, writer)?;
        writer.write_all(b")")?;
    }
    writer.write_all(b" ")
}

/// Renders the body of one CTE (inside its `AS ( … )`). A recursive CTE — a `Set` body that references the
/// CTE's own name — renders its `<anchor> UNION [ALL] <recursive>` arms per-arm: a plain tail-less arm
/// **bare** (a recursive-CTE grammar requires it — SQLite rejects *any* parenthesized recursive arm, and
/// the bare form is the SQL-standard shape PostgreSQL/MySQL accept too), a scoped arm parenthesized where
/// the dialect allows it (see [`render_recursive_arm`]). Any other CTE body renders via [`render_body`]
/// (which wraps set operands per the dialect). A CTE with no declared column list has its outputs named by
/// the body's projection aliases (`alias_projections`), exactly like a column-less `CREATE VIEW`.
///
/// `clause_recursive` is the clause-level `WITH RECURSIVE` flag (authoritative: `def.is_recursive()` on the
/// runtime path, `With.recursive` on a model). It gates self-reference detection because a CTE's own name is
/// in scope in its body **only** under `WITH RECURSIVE`: in a plain `WITH`, a body reference to a relation
/// that merely shares the CTE's bare name (e.g. a schemaless table named like the CTE) is an *outer* table,
/// not a self-reference, so such a CTE must render as a plain body — never routed to the recursive path.
fn render_cte_definition_body(
    cte: &CteModel,
    clause_recursive: bool,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    if clause_recursive && cte_is_self_referential(cte) {
        // A recursive CTE's body is a `UNION` set — possibly behind its own leading `WITH` prelude(s), e.g.
        // `WITH RECURSIVE c AS (WITH seed AS (…) <anchor> UNION ALL <recursive>)`. Recurse through those
        // leading `With` layers (rendering their prefixes normally) down to the recursive `Set`, whose arms
        // render per-arm (bare, or parenthesized where the dialect allows it) — not the generic set path
        // (whose SQLite `SELECT * FROM (…)` operand wrapping the recursive reference would make the view
        // fail to create).
        render_recursive_cte_set_body(&cte.body, dialect, writer)
    } else {
        render_body(&cte.body, cte.columns.is_empty(), dialect, writer)
    }
}

/// Renders the body of a **recursive** CTE. Leading `With` prelude(s) render normally (their own CTEs are
/// ordinary); the recursive `Set` at the core renders its arms per-arm (see [`render_recursive_arm`]).
/// A self-referential CTE whose core is not a `Set` cannot be a recursive CTE (which requires a `UNION`),
/// so it is rejected rather than mis-rendered.
fn render_recursive_cte_set_body(
    body: &ViewBody,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    match body {
        ViewBody::With {
            recursive,
            ctes,
            body,
        } => {
            render_with_prefix(*recursive, ctes, dialect, writer)?;
            render_recursive_cte_set_body(body, dialect, writer)
        }
        ViewBody::Set {
            op,
            all,
            left,
            right,
            order_by,
            limit,
            offset,
        } => {
            // A recursive CTE must connect its anchor and recursive term with `UNION`/`UNION ALL`; an
            // `INTERSECT`/`EXCEPT` over a self-reference is not a valid recursive CTE (SQLite rejects it as
            // a circular reference). Reject rather than emit invalid DDL.
            if *op != ViewSetOp::Union {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "a recursive CTE body must use UNION / UNION ALL — INTERSECT / EXCEPT cannot connect a \
                     recursive term",
                ));
            }
            // Each arm renders per-arm via [`OperandWrap::RecursiveArm`]: a plain tail-less arm bare, a
            // scoped arm (own `ORDER BY`/`LIMIT`/`OFFSET` or a nested compound) parenthesized where the
            // dialect permits it (PostgreSQL/MySQL) and rejected where it does not (SQLite). See
            // [`render_recursive_arm`].
            render_set(
                *op,
                *all,
                left,
                right,
                order_by,
                *limit,
                *offset,
                OperandWrap::RecursiveArm,
                dialect,
                writer,
            )
        }
        ViewBody::Select(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a recursive CTE body must be a UNION set operation (a self-referential CTE with a plain \
             SELECT body is not a valid recursive CTE)",
        )),
    }
}

/// Whether a CTE's body references the CTE's own name — the structural mark of a recursive CTE, which
/// governs its operand rendering (see [`render_with_prefix`]). Names in a `WITH` scope are unqualified,
/// so a bare-name match on any [`SourceItem::Named`] reachable from the body is correct.
fn cte_is_self_referential(cte: &CteModel) -> bool {
    fn body_references(body: &ViewBody, name: &str) -> bool {
        match body {
            ViewBody::Select(query) => query_references(query, name),
            ViewBody::Set { left, right, .. } => {
                body_references(left, name) || body_references(right, name)
            }
            ViewBody::With { ctes, body, .. } => {
                // An inner `WITH` that re-binds this name shadows the outer CTE — its references are not
                // self-references of the outer CTE. Only descend where the name is not locally rebound.
                if ctes.iter().any(|inner| inner.name == name) {
                    false
                } else {
                    ctes.iter().any(|inner| body_references(&inner.body, name))
                        || body_references(body, name)
                }
            }
        }
    }
    fn query_references(query: &ViewQueryModel, name: &str) -> bool {
        query
            .from
            .as_ref()
            .is_some_and(|source| source_references(source, name))
            || query
                .joins
                .iter()
                .any(|join| source_references(&join.source, name))
    }
    fn source_references(source: &SourceItem, name: &str) -> bool {
        match source {
            // A CTE self-reference is always unqualified; a schema-qualified relation (`public.counter`)
            // is a real table/view even if it shares the CTE's bare name, so it is NOT a self-reference.
            // Misclassifying it would render this non-recursive CTE's set body with bare arms (dropping the
            // dialect operand wrapping a per-arm `ORDER BY`/`LIMIT` or nested compound needs).
            SourceItem::Named(named) => named.schema.is_none() && named.name == name,
            SourceItem::Derived { query, .. } => body_references(query, name),
        }
    }
    body_references(&cte.body, &cte.name)
}

/// The SQL keyword(s) for a view-body set operator, e.g. `UNION` / `UNION ALL` / `INTERSECT`.
fn set_op_keyword(op: ViewSetOp, all: bool) -> &'static str {
    match (op, all) {
        (ViewSetOp::Union, false) => "UNION",
        (ViewSetOp::Union, true) => "UNION ALL",
        (ViewSetOp::Intersect, false) => "INTERSECT",
        (ViewSetOp::Intersect, true) => "INTERSECT ALL",
        (ViewSetOp::Except, false) => "EXCEPT",
        (ViewSetOp::Except, true) => "EXCEPT ALL",
    }
}

/// Writes a `FROM`/`JOIN` source. A named relation renders `<qualified> AS <alias>`; a derived table
/// renders `(<subquery>) AS <alias>` with its projections aliased so its output columns are named. The
/// alias is emitted unquoted so it matches the column references inside expressions, which qualify with
/// the bare alias.
fn render_source(
    source: &SourceItem,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    match source {
        SourceItem::Named(named) => {
            render_qualified(named.schema.as_deref(), &named.name, dialect, writer)?;
            write!(writer, " AS {}", named.alias)
        }
        SourceItem::Derived { query, alias } => {
            writer.write_all(b"(")?;
            render_body(query, true, dialect, writer)?;
            write!(writer, ") AS {alias}")
        }
    }
}

/// Renders a single scalar [`ExprNode`] to SQL for the given dialect — the entry point for the
/// expressions that live outside a view body: `CHECK` constraints, generated-column definitions, and
/// index key / partial-predicate terms. These carry **unqualified** column references
/// ([`ExprNode::Column`] with `alias: None`), rendered as bare quoted identifiers.
///
/// This is the render half that the reverse parser ([`squealy_parse`](https://docs.rs/squealy-parse)'s
/// `lower_expr`) inverts; the two must stay symmetric so a published constraint re-plans to empty.
pub fn render_scalar_expr(
    node: &ExprNode,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    render_expr(node, dialect, writer)
}

fn render_expr(node: &ExprNode, dialect: &dyn Dialect, writer: &mut dyn Write) -> io::Result<()> {
    match node {
        ExprNode::Column { alias, column } => {
            write!(writer, "{alias}.")?;
            dialect.write_quoted_ident(column, writer)
        }
        ExprNode::BareColumn { column } => dialect.write_quoted_ident(column, writer),
        ExprNode::Literal(text) => writer.write_all(text.as_bytes()),
        // The un-modelable escape hatch: emit the already-rendered dialect SQL verbatim.
        ExprNode::Raw(text) => writer.write_all(text.as_bytes()),
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
            // A general authored cast must render faithfully (its precision/scale is the semantics),
            // unlike the result-pin casts below which use `write_cast_type`. See git-bug 8fe1530.
            dialect.write_general_cast_type(ty, writer)?;
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
            frame,
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
            let mut wrote = false;
            if !partition_by.is_empty() {
                writer.write_all(b"PARTITION BY ")?;
                for (index, partition) in partition_by.iter().enumerate() {
                    if index > 0 {
                        writer.write_all(b", ")?;
                    }
                    render_expr(partition, dialect, writer)?;
                }
                wrote = true;
            }
            if !order_by.is_empty() {
                if wrote {
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
                wrote = true;
            }
            if let Some(frame) = frame {
                if wrote {
                    writer.write_all(b" ")?;
                }
                frame.render(writer)?;
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
            // SQLite spells substring as the comma-argument call `substr(s, start, len)` — it has no
            // `SUBSTRING(s FROM start FOR len)` syntax (same 1-based `start` as the standard form).
            ScalarFunc::Substring if args.len() == 3 && dialect.substring_uses_function_call() => {
                writer.write_all(b"substr(")?;
                render_expr(&args[0], dialect, writer)?;
                writer.write_all(b", ")?;
                render_expr(&args[1], dialect, writer)?;
                writer.write_all(b", ")?;
                render_expr(&args[2], dialect, writer)?;
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
                writer.write_all(scalar_func_name(*func, dialect).as_bytes())?;
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
        // A general function call renders its name verbatim (stored lowercased; no cross-dialect name
        // mapping) — `<name>(<arg>, …)`.
        ExprNode::Function { name, args } => {
            writer.write_all(name.as_bytes())?;
            writer.write_all(b"(")?;
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    writer.write_all(b", ")?;
                }
                render_expr(arg, dialect, writer)?;
            }
            writer.write_all(b")")
        }
        ExprNode::Now => {
            // Match the query renderer: MySQL needs `CURRENT_TIMESTAMP(6)` so a `now()` in a view/CTE
            // body keeps its microseconds (see `Dialect::now_fractional_digits`).
            writer.write_all(b"CURRENT_TIMESTAMP")?;
            if let Some(digits) = dialect.now_fractional_digits() {
                write!(writer, "({digits})")?;
            }
            Ok(())
        }
        ExprNode::Extract {
            field,
            operand,
            result,
            timezone,
        } => {
            // The native EXTRACT type differs by dialect, so it is cast to `result` (when set).
            // `Second` is floored to the whole-seconds component (PostgreSQL's is fractional; see
            // `render.rs`).
            let floor = *field == DateField::Second;
            if result.is_some() {
                writer.write_all(b"CAST(")?;
            }
            if floor {
                writer.write_all(b"FLOOR(")?;
            }
            writer.write_all(b"EXTRACT(")?;
            writer.write_all(field.extract_keyword().as_bytes())?;
            writer.write_all(b" FROM ")?;
            render_operand_at_time_zone(operand, timezone.as_deref(), dialect, writer)?;
            writer.write_all(b")")?;
            if floor {
                writer.write_all(b")")?;
            }
            if let Some(ty) = result {
                writer.write_all(b" AS ")?;
                dialect.write_cast_type(ty, writer)?;
                writer.write_all(b")")?;
            }
            Ok(())
        }
        ExprNode::DateTrunc {
            unit,
            operand,
            timezone,
        } => {
            // PostgreSQL only; a MySQL view carrying this fails at DDL exec (like a `full_join` view).
            writer.write_all(b"date_trunc('")?;
            writer.write_all(unit.trunc_literal().as_bytes())?;
            writer.write_all(b"', ")?;
            render_expr(operand, dialect, writer)?;
            // The 3-argument `date_trunc('unit', ts, 'tz')` truncates in `tz` and returns a
            // `timestamptz` (DST-correct; see the note in `render.rs`).
            if let Some(tz) = timezone {
                writer.write_all(b", '")?;
                writer.write_all(tz.replace('\'', "''").as_bytes())?;
                writer.write_all(b"'")?;
            }
            writer.write_all(b")")
        }
        ExprNode::ExtractSecond { operand, result } => {
            // Fractional seconds: PostgreSQL `EXTRACT(SECOND …)` vs MySQL composite
            // `EXTRACT(SECOND_MICROSECOND …) / 1000000.0` (see `render.rs`).
            let micro = dialect.extract_second_uses_microsecond_unit();
            if result.is_some() {
                writer.write_all(b"CAST(")?;
            }
            writer.write_all(b"EXTRACT(")?;
            writer.write_all(if micro {
                b"SECOND_MICROSECOND".as_slice()
            } else {
                b"SECOND".as_slice()
            })?;
            writer.write_all(b" FROM ")?;
            render_expr(operand, dialect, writer)?;
            writer.write_all(b")")?;
            if micro {
                writer.write_all(b" / 1000000.0")?;
            }
            if let Some(ty) = result {
                writer.write_all(b" AS ")?;
                dialect.write_cast_type(ty, writer)?;
                writer.write_all(b")")?;
            }
            Ok(())
        }
    }
}

/// Render an `extract`/`date_trunc` operand, wrapped in `(<operand> AT TIME ZONE '<tz>')` when a
/// timezone is set (embedded single quotes doubled). PostgreSQL only.
fn render_operand_at_time_zone(
    operand: &ExprNode,
    timezone: Option<&str>,
    dialect: &dyn Dialect,
    writer: &mut dyn Write,
) -> io::Result<()> {
    match timezone {
        Some(tz) => {
            writer.write_all(b"(")?;
            render_expr(operand, dialect, writer)?;
            writer.write_all(b" AT TIME ZONE '")?;
            writer.write_all(tz.replace('\'', "''").as_bytes())?;
            writer.write_all(b"')")
        }
        None => render_expr(operand, dialect, writer),
    }
}

/// SQL name for a [`ScalarFunc`] builtin in `dialect`'s spelling. The four unary string functions route
/// through the same [`Dialect::unary_string_fn_name`] seam the query renderer uses, so a backend that
/// respells one (e.g. SQLite `Length` -> `length`, not `CHAR_LENGTH`) fixes it once for both paths.
/// `Concat`/`Substring` reach here only in their default form (a pipe-`||` concat and a function-call
/// substring are handled by their own seams above), so their standard names suffice.
fn scalar_func_name(func: ScalarFunc, dialect: &dyn Dialect) -> &'static str {
    match func {
        ScalarFunc::Lower => dialect.unary_string_fn_name(UnaryStringFunc::Lower),
        ScalarFunc::Upper => dialect.unary_string_fn_name(UnaryStringFunc::Upper),
        ScalarFunc::Length => dialect.unary_string_fn_name(UnaryStringFunc::Length),
        ScalarFunc::Trim => dialect.unary_string_fn_name(UnaryStringFunc::Trim),
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
        ArithmeticOp::Modulo => "%",
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
