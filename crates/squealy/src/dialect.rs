use std::io::{self, Write};

use crate::{OrderNulls, RowLock, SqlType};

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

    /// Writes a `NULLS FIRST`/`NULLS LAST` ordering modifier (with a leading space) for an `ORDER BY`
    /// term in a view body.
    ///
    /// The default emits the standard modifier, which PostgreSQL supports. MySQL has no such syntax
    /// and overrides this to drop it (its `NULL`s already sort lowest), so a view carrying an explicit
    /// null-ordering still renders to valid MySQL DDL.
    fn write_order_nulls(&self, nulls: OrderNulls, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(match nulls {
            OrderNulls::First => b" NULLS FIRST",
            OrderNulls::Last => b" NULLS LAST",
        })
    }

    /// Whether a query-builder `ORDER BY … NULLS FIRST/LAST` term must be emulated rather than rendered
    /// with native syntax. The default is `false` (PostgreSQL renders ` NULLS FIRST/LAST`). MySQL has no
    /// such syntax and overrides this to `true`; the renderer then emits a leading `(<expr> IS NULL)`
    /// sort key instead.
    fn emulates_order_nulls(&self) -> bool {
        false
    }

    /// Writes a `SELECT … FOR UPDATE` / `FOR SHARE` row-locking clause (with a leading space).
    ///
    /// The default is the standard SQL spelling (PostgreSQL). MySQL has no `FOR SHARE` keyword and
    /// overrides the shared mode to `LOCK IN SHARE MODE`.
    fn write_row_lock(&self, lock: RowLock, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(match lock {
            RowLock::Update => b" FOR UPDATE",
            RowLock::Share => b" FOR SHARE",
        })
    }

    /// Whether string concatenation renders as the `||` operator (`a || b`) rather than `CONCAT(a, b)`.
    ///
    /// The two are not interchangeable: `||` propagates `NULL` (the result is `NULL` if any operand
    /// is), matching the builder's "nullable iff any operand is nullable" model, whereas PostgreSQL's
    /// `CONCAT` *ignores* `NULL` operands. The default is `false` (`CONCAT`), which is correct for
    /// MySQL (whose `CONCAT` propagates `NULL`, and whose `||` is logical OR). PostgreSQL overrides
    /// this to `true` so `||` is used — which also lets it infer a bare parameter's type (its `CONCAT`
    /// signature is `"any"` and cannot).
    fn concat_uses_pipe_operator(&self) -> bool {
        false
    }

    /// Whether `SUBSTRING(s FROM start FOR len)` needs its `start`/`len` bounds cast to `integer`.
    ///
    /// PostgreSQL overrides this to `true`: a bare parameter there is untyped, and
    /// `SUBSTRING(text FROM unknown FOR unknown)` resolves to the regex `substring(text FROM pattern
    /// FOR escape)` overload rather than the positional form, so the bounds must be cast to integer.
    /// The default is `false` — MySQL binds `?` by value (no inference) and has no regex overload, so
    /// it needs no cast (and its `CAST` vocabulary has no `INT` target anyway).
    fn substring_bounds_need_cast(&self) -> bool {
        false
    }

    /// Whether a bare literal/parameter operand of `EXTRACT`/`date_trunc` must be cast to its timestamp
    /// type.
    ///
    /// PostgreSQL overrides this to `true`: a bare parameter is untyped, and both `EXTRACT` and
    /// `date_trunc` are overloaded across `timestamp`/`timestamptz`/`interval`, so the server cannot
    /// resolve the placeholder when preparing the statement. The default is `false` — MySQL binds `?`
    /// by value (no inference). A *column* operand is already typed, so only bare literals/params
    /// (`ExprAst::NEEDS_CAST_ANCHOR`) are cast.
    fn timestamp_operand_needs_cast(&self) -> bool {
        false
    }

    /// Whether fractional-seconds `extract_second` must use the composite `SECOND_MICROSECOND` unit.
    ///
    /// PostgreSQL's `EXTRACT(SECOND FROM ts)` is already fractional, so the default (`false`) renders
    /// `EXTRACT(SECOND FROM ts)`. MySQL's `EXTRACT(SECOND …)` is integer-only, so it overrides this to
    /// `true` and the renderer uses `EXTRACT(SECOND_MICROSECOND FROM ts) / 1000000.0` (which references
    /// the operand once, returning `SSffffff`), matching PostgreSQL's fractional value.
    fn extract_second_uses_microsecond_unit(&self) -> bool {
        false
    }

    /// Writes a reference to the conflicting (proposed) row's column inside an upsert's `DO UPDATE SET`.
    ///
    /// PostgreSQL exposes it as `EXCLUDED."col"` (the default here). MySQL's `ON DUPLICATE KEY UPDATE`
    /// uses `VALUES(\`col\`)` / `new.col` instead and overrides this.
    fn write_excluded_column(&self, column: &str, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(b"EXCLUDED.")?;
        self.write_quoted_ident(column, writer)
    }

    /// Writes the prefix of an upsert's replace-all update, up to and including the keyword that
    /// introduces the `col = <excluded>` assignment list. The renderer then emits that list (shared
    /// across dialects) using [`write_excluded_column`](Self::write_excluded_column).
    ///
    /// PostgreSQL: ` ON CONFLICT (<target>) DO UPDATE SET ` (the default here). MySQL overrides it with
    /// ` ON DUPLICATE KEY UPDATE ` and ignores the target (it matches on every PK/UNIQUE key).
    fn write_upsert_set_prefix(&self, target: &[&str], writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(b" ON CONFLICT (")?;
        for (i, column) in target.iter().enumerate() {
            if i > 0 {
                writer.write_all(b", ")?;
            }
            self.write_quoted_ident(column, writer)?;
        }
        writer.write_all(b") DO UPDATE SET ")
    }

    /// Writes an upsert's "do nothing on conflict" clause.
    ///
    /// PostgreSQL has a first-class ` ON CONFLICT (<target>) DO NOTHING` (the default here). MySQL has
    /// no `DO NOTHING`, so it emulates one by self-assigning the first inserted column
    /// (` ON DUPLICATE KEY UPDATE \`c\` = \`c\``); `first_column` is `None` only for a column-less
    /// (`DEFAULT VALUES`) insert, which MySQL cannot express as a no-op upsert.
    fn write_upsert_do_nothing(
        &self,
        target: &[&str],
        first_column: Option<&str>,
        writer: &mut dyn Write,
    ) -> io::Result<()> {
        let _ = first_column;
        writer.write_all(b" ON CONFLICT (")?;
        for (i, column) in target.iter().enumerate() {
            if i > 0 {
                writer.write_all(b", ")?;
            }
            self.write_quoted_ident(column, writer)?;
        }
        writer.write_all(b") DO NOTHING")
    }
}
