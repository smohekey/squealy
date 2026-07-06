//! AST → neutral-model lowering — the structural inverse of the renderers.
//!
//! The renderers walk [`squealy::ExprNode`] / [`squealy::ViewQueryModel`] into dialect SQL
//! ([`squealy::view_render`] and each backend's DDL writer). Lowering walks the [`sqlparser`] AST the
//! other way. It is dialect-parameterized by [`SqlDialect`] because the same syntax can mean different
//! things across dialects (`||` is concatenation in PostgreSQL/SQLite but logical `OR` in MySQL), and
//! because inverting the renderer's per-dialect idioms requires knowing which dialect emitted them.
//!
//! Lowering leans on [`crate::normalize`] to fold dialect spellings and unwind render idioms before
//! (or while) building the neutral node.
//!
//! # Status (Phase 0)
//!
//! Not yet implemented: [`lower_expr`] returns [`ReadError::NotYetLowered`]. The closed-IR expression
//! lowering (checks / generated / index expressions) is the first phase to fill this in, followed by
//! view-body lowering ([`lower_query`]).

use sqlparser::ast::{Expr, Query};
use squealy::{ExprNode, ViewQueryModel};

use crate::{ReadError, SqlDialect};

/// Lowers a parsed scalar expression into an [`ExprNode`].
///
/// Phase 0 stub: returns [`ReadError::NotYetLowered`] describing the node it received, so callers (and
/// the round-trip harness) can see exactly which shapes remain to be handled.
pub fn lower_expr(expr: &Expr, dialect: SqlDialect) -> Result<ExprNode, ReadError> {
    let _ = dialect;
    Err(ReadError::NotYetLowered(format!(
        "scalar expression `{expr}`"
    )))
}

/// Lowers a parsed `SELECT` query (a view body) into a [`ViewQueryModel`].
///
/// Phase 0 stub: returns [`ReadError::NotYetLowered`]. View-body reconstruction (projection/from/
/// joins/filter + view-output type inference) is a later phase.
pub fn lower_query(query: &Query, dialect: SqlDialect) -> Result<ViewQueryModel, ReadError> {
    let _ = dialect;
    Err(ReadError::NotYetLowered(format!("query body `{query}`")))
}
