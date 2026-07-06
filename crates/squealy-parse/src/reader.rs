//! The per-dialect reader seam — SQL text into neutral-model objects.
//!
//! [`Reader`] mirrors the renderer's structure: where each backend renders the neutral model to SQL
//! via a [`squealy::Dialect`], a `Reader` inverts that for one [`SqlDialect`]. The entry points here
//! correspond one-for-one to the render entry points a round-trip must invert:
//!
//! | render (out)                                   | read (in)                        |
//! |------------------------------------------------|----------------------------------|
//! | [`squealy::render_create_view`]                | [`Reader::read_create_view`]     |
//! | backend DDL writer — `CHECK (<expr>)`          | [`Reader::read_check_expression`]     |
//! | backend DDL writer — `GENERATED ALWAYS AS (…)` | [`Reader::read_generated_expression`] |
//! | backend DDL writer — index key term            | [`Reader::read_index_expression`]     |
//!
//! During Phase 0 these parse the input (proving `sqlparser` accepts squealy's rendered output) and
//! then return [`ReadError::NotYetLowered`]; the lowering is filled in per phase.

use sqlparser::ast::Statement;
use squealy::{ExprNode, ViewModel};

use crate::{ReadError, SqlDialect, lower, parse_expr, parse_sql};

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

    /// Reads a `CREATE VIEW` statement into a [`ViewModel`] (the inverse of
    /// [`squealy::render_create_view`]).
    ///
    /// Phase 0: verifies the text parses to a single `CREATE VIEW`, then returns
    /// [`ReadError::NotYetLowered`] — view-body reconstruction lands in a later phase.
    pub fn read_create_view(&self, sql: &str) -> Result<ViewModel, ReadError> {
        let statements = parse_sql(sql, self.dialect)?;
        match statements.as_slice() {
            [Statement::CreateView(_)] => Err(ReadError::NotYetLowered(
                "CREATE VIEW body reconstruction".to_owned(),
            )),
            [other] => Err(ReadError::Unexpected(format!(
                "expected a single CREATE VIEW statement, found: {other}"
            ))),
            stmts => Err(ReadError::Unexpected(format!(
                "expected a single CREATE VIEW statement, found {} statement(s)",
                stmts.len()
            ))),
        }
    }

    /// Reads a table `CHECK` constraint's boolean expression into an [`ExprNode`].
    pub fn read_check_expression(&self, sql: &str) -> Result<ExprNode, ReadError> {
        self.read_scalar_expression(sql)
    }

    /// Reads a generated/computed column's defining expression into an [`ExprNode`].
    pub fn read_generated_expression(&self, sql: &str) -> Result<ExprNode, ReadError> {
        self.read_scalar_expression(sql)
    }

    /// Reads an index key term's expression into an [`ExprNode`].
    pub fn read_index_expression(&self, sql: &str) -> Result<ExprNode, ReadError> {
        self.read_scalar_expression(sql)
    }

    /// Shared path for the scalar-expression entry points: parse a single expression, then lower it.
    fn read_scalar_expression(&self, sql: &str) -> Result<ExprNode, ReadError> {
        let expr = parse_expr(sql, self.dialect)?;
        lower::lower_expr(&expr, self.dialect)
    }
}
