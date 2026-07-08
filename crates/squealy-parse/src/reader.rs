//! The per-dialect reader seam — SQL text into neutral-model objects.
//!
//! [`Reader`] mirrors the renderer's structure: where each backend renders the neutral model to SQL
//! via a `Dialect`, a `Reader` inverts that for one [`SqlDialect`]. The entry points here
//! correspond one-for-one to the render entry points a round-trip must invert:
//!
//! | render (out)                                   | read (in)                             |
//! |------------------------------------------------|---------------------------------------|
//! | `render_create_view`                           | [`Reader::read_create_view`]          |
//! | (a backend's `pg_get_viewdef` / `VIEW_DEFINITION` deparse) | [`Reader::read_view_query`] |
//! | backend DDL writer — `CHECK (<expr>)`          | [`Reader::read_check_expression`]     |
//! | backend DDL writer — `GENERATED ALWAYS AS (…)` | [`Reader::read_generated_expression`] |
//! | backend DDL writer — index key term            | [`Reader::read_index_expression`]     |
//!
//! The scalar entry points lower structurally; the view-body entry points reconstruct a single-`SELECT`
//! body into a [`ViewQueryModel`] (the structural inverse of the SELECT renderer). Shapes still outside
//! the lowering grammar return [`ReadError::NotYetLowered`].

use sqlparser::ast::Statement;
use squealy_ir::{ExprNode, ViewQueryModel};

use crate::{ReadError, SqlDialect, lower, parse_expr, parse_expr_list, parse_sql};

/// Reads dialect SQL text back into neutral-model objects for a single [`SqlDialect`].
///
/// See the [module docs](self) for the correspondence with the renderer.
#[derive(Debug, Clone, Copy)]
pub struct Reader {
    dialect: SqlDialect,
}

impl Reader {
    /// Creates a reader for the given input dialect.
    pub fn new(dialect: SqlDialect) -> Self {
        Reader { dialect }
    }

    /// The dialect this reader parses.
    pub fn dialect(&self) -> SqlDialect {
        self.dialect
    }

    /// Reads a `CREATE VIEW` statement into its body [`ViewQueryModel`] — the structural inverse of
    /// `render_create_view`.
    ///
    /// The returned model is the view's `SELECT` *body* only: the view's output column *types* are not
    /// present in the SQL text (a column list gives at most names), so the backend supplies the typed
    /// output columns from its catalog to assemble a full `ViewModel`. When a `CREATE VIEW (cols) AS …`
    /// column list is present, its names identify the (un-aliased) projection outputs positionally.
    ///
    /// A body outside the single-`SELECT` grammar the SELECT renderer emits (set operations, CTEs,
    /// derived-table sources, comma joins, …) returns [`ReadError::NotYetLowered`].
    pub fn read_create_view(&self, sql: &str) -> Result<ViewQueryModel, ReadError> {
        let statements = parse_sql(sql, self.dialect)?;
        match statements.as_slice() {
            [Statement::CreateView(create_view)] => {
                lower::lower_create_view(create_view, self.dialect)
            }
            [other] => Err(ReadError::Unexpected(format!(
                "expected a single CREATE VIEW statement, found: {other}"
            ))),
            stmts => Err(ReadError::Unexpected(format!(
                "expected a single CREATE VIEW statement, found {} statement(s)",
                stmts.len()
            ))),
        }
    }

    /// Reads a bare `SELECT` view definition (a `Statement::Query`) into a [`ViewQueryModel`] — the form
    /// a backend's view-body deparse returns (PostgreSQL's `pg_get_viewdef`, MySQL's
    /// `information_schema.VIEWS.VIEW_DEFINITION`). Unlike [`read_create_view`] there is no surrounding
    /// `CREATE VIEW (cols)` column list, so the projection outputs are named by their own `AS` aliases
    /// (or a bare column's name).
    pub fn read_view_query(&self, sql: &str) -> Result<ViewQueryModel, ReadError> {
        let statements = parse_sql(sql, self.dialect)?;
        match statements.as_slice() {
            [Statement::Query(query)] => lower::lower_query(query, self.dialect),
            [other] => Err(ReadError::Unexpected(format!(
                "expected a single SELECT query, found: {other}"
            ))),
            stmts => Err(ReadError::Unexpected(format!(
                "expected a single SELECT query, found {} statement(s)",
                stmts.len()
            ))),
        }
    }

    /// Reads a table `CHECK` constraint's boolean expression into an [`ExprNode`].
    pub fn read_check_expression(&self, sql: &str) -> Result<ExprNode, ReadError> {
        self.read_scalar_expression(sql)
    }

    /// Reads a `CHECK` expression into a structural [`ExprNode`], falling back to
    /// [`ExprNode::Raw`] carrying the input text when it cannot be parsed or lowered.
    ///
    /// This is the live-introspection entry point: a backend's deparse output must always yield *some*
    /// `ExprNode` (the model field is structural), so an expression the reverse parser cannot yet
    /// structure is preserved verbatim rather than dropped. A `Raw` result will not compare equal to a
    /// structural desired node — i.e. it churns — so extending the lowering shrinks the set that lands
    /// here.
    pub fn read_check_expression_or_raw(&self, sql: &str) -> ExprNode {
        self.read_check_expression(sql)
            .unwrap_or_else(|_| ExprNode::Raw(sql.to_owned()))
    }

    /// Reads a generated/computed column's defining expression into an [`ExprNode`].
    pub fn read_generated_expression(&self, sql: &str) -> Result<ExprNode, ReadError> {
        self.read_scalar_expression(sql)
    }

    /// Reads a generated-column expression into a structural [`ExprNode`], falling back to
    /// [`ExprNode::Raw`] carrying the input text when it cannot be parsed or lowered — the
    /// live-introspection entry point (see [`read_check_expression_or_raw`](Self::read_check_expression_or_raw)).
    pub fn read_generated_expression_or_raw(&self, sql: &str) -> ExprNode {
        self.read_generated_expression(sql)
            .unwrap_or_else(|_| ExprNode::Raw(sql.to_owned()))
    }

    /// Reads an index key term's expression into an [`ExprNode`].
    pub fn read_index_expression(&self, sql: &str) -> Result<ExprNode, ReadError> {
        self.read_scalar_expression(sql)
    }

    /// Reads an index key term's expression into a structural [`ExprNode`], falling back to
    /// [`ExprNode::Raw`] carrying the input text when it cannot be parsed or lowered — the
    /// live-introspection entry point (see [`read_check_expression_or_raw`](Self::read_check_expression_or_raw)).
    pub fn read_index_expression_or_raw(&self, sql: &str) -> ExprNode {
        self.read_index_expression(sql)
            .unwrap_or_else(|_| ExprNode::Raw(sql.to_owned()))
    }

    /// Reads the expression key terms of an index into one structural [`ExprNode`] **per term**, splitting
    /// the comma-separated list PostgreSQL's `pg_get_expr(indexprs, …)` returns for a multi-expression
    /// index (`lower(a), upper(b)` → two nodes). This is the live-introspection entry point for the whole
    /// expression key: each element must lower structurally for the split to be taken; if the list does not
    /// parse, or any element cannot be lowered (e.g. a `::text` cast on a non-literal), the entire input is
    /// preserved as a single verbatim [`ExprNode::Raw`] so it re-renders unchanged and both sides of a diff
    /// still compare equal.
    pub fn read_index_expressions_or_raw(&self, sql: &str) -> Vec<ExprNode> {
        let lowered = parse_expr_list(sql, self.dialect).ok().and_then(|exprs| {
            exprs
                .iter()
                .map(|expr| lower::lower_expr(expr, self.dialect).ok())
                .collect::<Option<Vec<_>>>()
        });
        lowered.unwrap_or_else(|| vec![ExprNode::Raw(sql.to_owned())])
    }

    /// Reads a partial-index predicate (the boolean `WHERE` of a `CREATE INDEX`) into an [`ExprNode`].
    pub fn read_index_predicate(&self, sql: &str) -> Result<ExprNode, ReadError> {
        self.read_scalar_expression(sql)
    }

    /// Reads a partial-index predicate into a structural [`ExprNode`], falling back to [`ExprNode::Raw`]
    /// carrying the input text when it cannot be parsed or lowered — the live-introspection entry point
    /// (see [`read_check_expression_or_raw`](Self::read_check_expression_or_raw)).
    pub fn read_index_predicate_or_raw(&self, sql: &str) -> ExprNode {
        self.read_index_predicate(sql)
            .unwrap_or_else(|_| ExprNode::Raw(sql.to_owned()))
    }

    /// Shared path for the scalar-expression entry points: parse a single expression, then lower it.
    fn read_scalar_expression(&self, sql: &str) -> Result<ExprNode, ReadError> {
        let expr = parse_expr(sql, self.dialect)?;
        lower::lower_expr(&expr, self.dialect)
    }
}
