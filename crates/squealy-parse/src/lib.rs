//! Reverse SQL parsing for squealy: dialect SQL text back into the neutral schema model.
//!
//! squealy is otherwise a one-way compiler — the neutral model (`DatabaseModel`,
//! [`squealy_ir::ViewQueryModel`], [`squealy_ir::ExprNode`]) is rendered *out* to dialect SQL by the
//! renderers in `view_render` and each backend's DDL writer. This crate is the missing
//! inverse: it parses dialect SQL and lowers it *into* that neutral model, so a live database of any
//! dialect can be introspected structurally (view bodies, check / generated / index expressions) and
//! re-rendered to any other dialect.
//!
//! # Architecture
//!
//! Parsing is delegated to [`sqlparser`] (pinned — see the crate `Cargo.toml`), which produces a
//! faithful, dialect-spelled AST but does **zero** cross-dialect canonicalization. The real work lives
//! in two squealy-owned layers:
//!
//! - [`lower`] — AST → [`squealy_ir::ExprNode`] / [`squealy_ir::ViewQueryModel`]. The structural inverse of
//!   the renderers, dialect-parameterized the same way they are.
//! - [`normalize`] — semantics-preserving folding of dialect spellings to one neutral node (e.g.
//!   `||` ↔ `CONCAT`, `substr` ↔ `SUBSTRING … FROM … FOR`, `CURRENT_TIMESTAMP(n)` ↔ `now()`), and the
//!   unwinding of the renderer's own idioms (full parenthesization, `CAST(<call> AS ty)` result-pins,
//!   the float-cast division form, the empty-`IN ()` sentinel). See that module for the full catalogue.
//!
//! [`Reader`] is the per-dialect entry seam mirroring the renderer.
//!
//! # Status (Phase 0 — foundation)
//!
//! This crate currently scaffolds the seam and pins the parser: the [`Reader`] entry points and the
//! [`parse_sql`] / [`parse_expr`] wrappers accept squealy's own rendered SQL and hand back the
//! `sqlparser` AST, but the AST → model lowering is not yet implemented and returns
//! [`ReadError::NotYetLowered`]. The `render → parse` half of the round-trip identity spine is
//! exercised by the harness in `squealy-model/tests/roundtrip.rs`. Lowering + normalization land in the
//! subsequent phases (checks / generated / index expressions first, then view bodies).

pub mod lower;
pub mod normalize;
pub mod reader;

pub use reader::Reader;

use sqlparser::ast::{Expr, Statement};
use sqlparser::dialect::{
    Dialect as ParserDialect, GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

// Re-export the pinned parser so downstream crates name AST types (`squealy_parse::sqlparser::ast::…`)
// against exactly the version this crate lowers from, without taking their own `sqlparser` dependency
// (and thereby risking a second, drifting copy in the graph).
pub use sqlparser;

/// Selects which SQL dialect's grammar and spelling conventions a [`Reader`] parses against.
///
/// Each variant maps to a [`sqlparser`] dialect. It also identifies which of the renderer's
/// per-dialect idioms (see `Dialect`) were used to produce the text, which the lowering
/// layer needs in order to invert them (e.g. whether `||` denotes string concatenation or logical
/// `OR`). The neutral model itself is dialect-independent; this only describes the *input* text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlDialect {
    /// PostgreSQL — the renderer's default shape (`"`-quoting, `ILIKE`, `||` concat, float-cast
    /// integer division, `CURRENT_TIMESTAMP` already microsecond).
    Postgres,
    /// MySQL — `` ` ``-quoting, `CONCAT`, `/` already floating-point, `CURRENT_TIMESTAMP(6)` for
    /// `now()`, `EXTRACT(SECOND_MICROSECOND …)`.
    Mysql,
    /// SQLite — unqualified names, `substr(s, a, b)`, `length`.
    Sqlite,
    /// A permissive superset used for text with no committed dialect (e.g. the synthetic test
    /// backend's output). Not a round-trip target on its own.
    Generic,
}

impl SqlDialect {
    /// The [`sqlparser`] dialect this maps to.
    fn parser_dialect(self) -> Box<dyn ParserDialect> {
        match self {
            SqlDialect::Postgres => Box::new(PostgreSqlDialect {}),
            SqlDialect::Mysql => Box::new(MySqlDialect {}),
            SqlDialect::Sqlite => Box::new(SQLiteDialect {}),
            SqlDialect::Generic => Box::new(GenericDialect {}),
        }
    }
}

/// An error reading dialect SQL back into the neutral model.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The text could not be parsed by `sqlparser` for the selected dialect.
    #[error("failed to parse {dialect:?} SQL: {source}")]
    Parse {
        /// The dialect the parser was configured for.
        dialect: SqlDialect,
        /// The underlying `sqlparser` error.
        #[source]
        source: ParserError,
    },
    /// The text parsed, but its shape is not one the reader expects at this entry point (e.g. a
    /// statement that is not a `CREATE VIEW` handed to [`Reader::read_create_view`]).
    #[error("unexpected SQL shape: {0}")]
    Unexpected(String),
    /// The text parsed into an AST that squealy cannot yet lower into the neutral model. This is the
    /// expected outcome during Phase 0 (the lowering is scaffolded but not implemented) and the marker
    /// for genuinely un-modelable constructs thereafter.
    #[error("parsed, but lowering into the neutral model is not yet implemented: {0}")]
    NotYetLowered(String),
}

/// Parses SQL text (one or more statements) into the `sqlparser` AST for the given dialect.
///
/// This is the thin parsing front-end shared by every [`Reader`] entry point; the lowering into the
/// neutral model happens on top of the returned AST.
pub fn parse_sql(sql: &str, dialect: SqlDialect) -> Result<Vec<Statement>, ReadError> {
    Parser::parse_sql(dialect.parser_dialect().as_ref(), sql)
        .map_err(|source| ReadError::Parse { dialect, source })
}

/// Parses a single scalar SQL expression (a check / generated-column / index expression, or a
/// projection/predicate fragment of a view body) into the `sqlparser` [`Expr`] AST.
///
/// The whole input must be one expression: `parse_expr` consumes only the leading expression, so any
/// trailing tokens (a second expression, a stray statement, junk) are rejected as
/// [`ReadError::Unexpected`] rather than silently truncating the read.
pub fn parse_expr(sql: &str, dialect: SqlDialect) -> Result<Expr, ReadError> {
    let binding = dialect.parser_dialect();
    let mut parser = Parser::new(binding.as_ref())
        .try_with_sql(sql)
        .map_err(|source| ReadError::Parse { dialect, source })?;
    let expr = parser
        .parse_expr()
        .map_err(|source| ReadError::Parse { dialect, source })?;
    // `parse_expr` stops at the first token it cannot fold into the expression; require EOF so a valid
    // prefix followed by extra input (`length(sku) > 0 junk`, `1; SELECT 2`) is an error, not a
    // truncated success.
    let trailing = &parser.peek_token().token;
    if *trailing != Token::EOF {
        return Err(ReadError::Unexpected(format!(
            "unexpected trailing tokens after expression: `{trailing}`"
        )));
    }
    Ok(expr)
}
