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
}
