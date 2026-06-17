use std::io::{self, Write};

use crate::SqlType;

/// The SQL-dialect differences the query renderer needs from a backend.
///
/// Rendering a `SELECT`/`INSERT`/`UPDATE`/`DELETE` from the query AST is otherwise identical across
/// the backends squealy targets, so the sink logic is shared and only these hooks vary by dialect.
/// PostgreSQL and MySQL differ here in exactly three places — bound-parameter placeholders, identifier
/// quoting, and the type name used inside a `CAST`.
pub trait Dialect {
    /// Writes the placeholder for the bound parameter at zero-based position `index`.
    ///
    /// PostgreSQL numbers parameters positionally (`$1`, `$2`, …); MySQL uses a bare `?`.
    fn write_placeholder(&self, index: usize, writer: &mut dyn Write) -> io::Result<()>;

    /// Writes a quoted identifier. PostgreSQL quotes with `"` and MySQL with `` ` ``; either way the
    /// quote character itself is escaped by doubling.
    fn write_quoted_ident(&self, ident: &str, writer: &mut dyn Write) -> io::Result<()>;

    /// Writes the type name for a `CAST(expr AS <type>)`. Dialects spell these differently — for
    /// example PostgreSQL's `double precision` versus MySQL's numeric cast types.
    fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()>;

    /// Whether `/` performs integer division when both operands are integers, so the renderer must
    /// cast operands to floating point to get the query builder's always-fractional division.
    ///
    /// PostgreSQL does integer division (so this is `true`); MySQL's `/` is already floating-point
    /// division (`false`), and casting would change `DECIMAL` results.
    fn integer_division_needs_float_cast(&self) -> bool {
        true
    }

    /// Writes a `LIMIT`/`OFFSET` clause. The default is the standard form, with `OFFSET` emittable on
    /// its own. MySQL accepts `OFFSET` only as part of `LIMIT`, so it overrides this to supply a
    /// sentinel limit for the offset-without-limit case.
    fn write_limit_offset(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
        writer: &mut dyn Write,
    ) -> io::Result<()> {
        if let Some(limit) = limit {
            write!(writer, " LIMIT {limit}")?;
        }
        if let Some(offset) = offset {
            write!(writer, " OFFSET {offset}")?;
        }
        Ok(())
    }

    /// Writes the values clause for an `INSERT` of a single all-default/auto-increment row (no
    /// explicit columns). PostgreSQL uses `DEFAULT VALUES`; MySQL uses `() VALUES ()`.
    fn write_default_row_insert(&self, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(b" DEFAULT VALUES")
    }

    /// Writes the `LIKE` operator (with surrounding spaces) for a pattern match, selecting the
    /// `NOT` form when `negated`.
    ///
    /// The default ignores `case_insensitive` and always emits `LIKE`, which is correct for MySQL
    /// where the default (case-insensitive) collations make `LIKE` case-insensitive already.
    /// PostgreSQL overrides this to emit `ILIKE` for case-insensitive matches.
    fn write_like_operator(
        &self,
        case_insensitive: bool,
        negated: bool,
        writer: &mut dyn Write,
    ) -> io::Result<()> {
        let _ = case_insensitive;
        writer.write_all(if negated { b" NOT LIKE " } else { b" LIKE " })
    }
}
