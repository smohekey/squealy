/// Renders a `TIME`/`TIMESTAMP`/`DATETIME` type with its optional fractional-seconds precision.
fn write_mysql_temporal(
	writer: &mut dyn Write,
	base: &str,
	precision: Option<u8>,
) -> io::Result<()> {
	writer.write_all(base.as_bytes())?;
	if let Some(precision) = precision {
		write!(writer, "({precision})")?;
	}
	Ok(())
}
/// Quotes an identifier with backticks, doubling any embedded backtick. Writes whole UTF-8 slices so
/// validating writers accept multibyte identifiers.
fn write_quoted_ident(value: &str, writer: &mut impl Write) -> io::Result<()> {
	write_delimited(value, '`', writer)
}

/// MySQL's [`Dialect`](squealy::Dialect): `?` placeholders, backtick-quoted identifiers, MySQL `CAST`
/// target types, and float division (so `/` needs no float cast). The shared core renderer
/// ([`squealy::render`]) drives MySQL query rendering through this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MysqlDialect;

impl squealy::Dialect for MysqlDialect {
	fn write_placeholder(&self, _index: usize, writer: &mut dyn Write) -> io::Result<()> {
		// MySQL placeholders are positional `?`, unnumbered.
		writer.write_all(b"?")
	}

	fn write_quoted_ident(&self, ident: &str, mut writer: &mut dyn Write) -> io::Result<()> {
		write_quoted_ident(ident, &mut writer)
	}

	/// MySQL has no `UPDATE … FROM`; a correlated update joins the source before `SET`
	/// (`UPDATE t JOIN other ON … SET …`).
	fn update_from_style(&self) -> squealy::UpdateFromStyle {
		squealy::UpdateFromStyle::MysqlJoin
	}

	// --- Upsert (`INSERT … ON DUPLICATE KEY UPDATE`) ---

	/// MySQL references the proposed row as `VALUES(\`col\`)` (vs PostgreSQL's `EXCLUDED."col"`). The
	/// 8.0.19+ row-alias form (`AS new … new.col`) is a follow-up; `VALUES()` is the widely-compatible
	/// spelling.
	fn write_excluded_column(&self, column: &str, writer: &mut dyn Write) -> io::Result<()> {
		writer.write_all(b"VALUES(")?;
		self.write_quoted_ident(column, writer)?;
		writer.write_all(b")")
	}

	/// MySQL has no conflict target — `ON DUPLICATE KEY UPDATE` matches on every PK/UNIQUE key — so the
	/// target is ignored and this is just the keyword that introduces the assignment list.
	fn write_upsert_set_prefix(&self, _target: &[&str], writer: &mut dyn Write) -> io::Result<()> {
		writer.write_all(b" ON DUPLICATE KEY UPDATE ")
	}

	/// MySQL has no `DO NOTHING`; emulate it by self-assigning a column (a no-op update). Prefer an
	/// inserted column; fall back to a conflict-target column for a column-less (`DEFAULT VALUES`)
	/// insert, which has no inserted column to assign. The conflict target always has at least one
	/// column (it comes from `on_conflict(|t| …)`), so the no-op clause is never silently dropped.
	fn write_upsert_do_nothing(
		&self,
		target: &[&str],
		first_column: Option<&str>,
		writer: &mut dyn Write,
	) -> io::Result<()> {
		if let Some(column) = first_column.or_else(|| target.first().copied()) {
			writer.write_all(b" ON DUPLICATE KEY UPDATE ")?;
			self.write_quoted_ident(column, writer)?;
			writer.write_all(b" = ")?;
			self.write_quoted_ident(column, writer)?;
		}
		Ok(())
	}

	fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
		// A user-defined enum is a PostgreSQL-only type; MySQL has no equivalent CAST target, and the
		// `_ => "CHAR"` fall-through below would silently rewrite the cast's semantics. Reject it (the
		// enum column class is already refused up front — this catches an enum buried in an expression).
		if let SqlType::Enum(name) = ty {
			return Err(io::Error::new(
				io::ErrorKind::Unsupported,
				format!("MySQL cannot render a CAST to the user-defined enum type `{name}`"),
			));
		}
		// `CAST(expr AS <type>)` accepts a restricted vocabulary in MySQL, distinct from column types
		// (e.g. `SIGNED`/`UNSIGNED`/`CHAR`, not `INT`/`VARCHAR`).
		let name = match ty {
			// 128-bit ints exceed MySQL's 64-bit `SIGNED`/`UNSIGNED`, so cast to a full-precision
			// decimal (e.g. a widened `SUM(BIGINT UNSIGNED)`) rather than overflowing.
			SqlType::I128 | SqlType::U128 => "DECIMAL(65, 0)",
			SqlType::Bool | SqlType::I8 | SqlType::I16 | SqlType::I32 | SqlType::I64 | SqlType::Isize => {
				"SIGNED"
			}
			SqlType::U8 | SqlType::U16 | SqlType::U32 | SqlType::U64 | SqlType::Usize => "UNSIGNED",
			// `CAST(x AS DECIMAL)` with no scale is `DECIMAL(10, 0)` and truncates the fraction, so
			// float results (e.g. `AVG`) cast to `DOUBLE` to stay fractional.
			SqlType::F32 | SqlType::F64 => "DOUBLE",
			SqlType::Decimal { .. } => "DECIMAL",
			SqlType::Date => "DATE",
			// A timestamp/time cast carries its fractional-seconds precision (`DATETIME(6)`), so a
			// `CASE`/`COALESCE` result feeding a `TIMESTAMP(6)` column keeps its microseconds rather than
			// being truncated to fsp 0. (`TIMESTAMP` is not a valid MySQL cast target — `DATETIME` is.)
			SqlType::Time { precision, .. } => {
				return write_mysql_temporal(writer, "TIME", *precision);
			}
			SqlType::Timestamp { precision, .. } => {
				return write_mysql_temporal(writer, "DATETIME", *precision);
			}
			// Both variable and fixed-width binary cast to `BINARY` so a binary expression operand in
			// `CASE`/`NULLIF`/`COALESCE` stays binary instead of being coerced through the text charset.
			SqlType::Bytes | SqlType::FixedBytes(_) => "BINARY",
			_ => "CHAR",
		};
		writer.write_all(name.as_bytes())
	}

	fn write_general_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
		squealy::reject_128bit_general_cast(ty)?;
		// A general authored cast spells the precision/scale faithfully — `CAST(x AS DECIMAL(10, 2))` —
		// unlike a result-pin cast (bare `DECIMAL`, whose exact type is recovered from the output column).
		// MySQL's `CAST` accepts `DECIMAL(M, D)`, so a cross-dialect-deployed decimal cast keeps its scale
		// and round-trips (the reverse parser's `general_cast` now structures MySQL `Decimal` casts). 8fe1530.
		if let SqlType::Decimal { precision, scale } = ty {
			// MySQL's DECIMAL is limited to 1 <= precision <= 65, scale <= 30, and scale <= precision. A
			// general cast outside that range (e.g. a PostgreSQL `numeric(100, 50)` in a cross-dialect
			// package, or a hand-built zero-precision decimal) has no faithful MySQL rendering — reject it at
			// the fidelity boundary rather than emit DDL that errors only at execution.
			if *precision == 0 || *precision > 65 || *scale > 30 || scale > precision {
				return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "MySQL cannot render a general CAST to DECIMAL({precision}, {scale}) \
                         (DECIMAL is limited to 1 <= precision <= 65, scale <= 30, and scale <= precision)"
                    ),
                ));
			}
			return write!(writer, "DECIMAL({precision}, {scale})");
		}
		self.write_cast_type(ty, writer)
	}

	fn integer_division_needs_float_cast(&self) -> bool {
		// MySQL `/` is always floating-point division; `DIV` is the integer form.
		false
	}

	fn now_fractional_digits(&self) -> Option<u8> {
		// MySQL's bare `CURRENT_TIMESTAMP` is fsp 0; the microsecond `now()` value types feed
		// `TIMESTAMP(6)` columns, so render `CURRENT_TIMESTAMP(6)` to keep the sub-seconds.
		Some(6)
	}

	fn write_limit_offset(
		&self,
		limit: Option<usize>,
		offset: Option<usize>,
		writer: &mut dyn Write,
	) -> io::Result<()> {
		// MySQL accepts OFFSET only as part of a LIMIT clause, so an offset-without-limit query needs
		// a sentinel limit (the documented `18446744073709551615` "all rows" value).
		match (limit, offset) {
			(Some(limit), Some(offset)) => write!(writer, " LIMIT {limit} OFFSET {offset}"),
			(Some(limit), None) => write!(writer, " LIMIT {limit}"),
			(None, Some(offset)) => write!(writer, " LIMIT 18446744073709551615 OFFSET {offset}"),
			(None, None) => Ok(()),
		}
	}

	fn write_default_row_insert(&self, writer: &mut dyn Write) -> io::Result<()> {
		// MySQL's empty-row insert form; `DEFAULT VALUES` is PostgreSQL-only.
		writer.write_all(b" () VALUES ()")
	}

	fn write_order_nulls(
		&self,
		_nulls: squealy::OrderNulls,
		_writer: &mut dyn Write,
	) -> io::Result<()> {
		// MySQL has no `NULLS FIRST`/`NULLS LAST` modifier, so a view carrying one drops it here
		// rather than emitting syntax MySQL rejects. (Query-builder `ORDER BY` instead emulates it via
		// `emulates_order_nulls` below, which the view-DDL path does not use.)
		Ok(())
	}

	fn emulates_order_nulls(&self) -> bool {
		// MySQL lacks `NULLS FIRST/LAST`; the renderer emits a leading `(<expr> IS NULL)` sort key.
		true
	}

	fn write_row_lock(&self, lock: squealy::RowLock, writer: &mut dyn Write) -> io::Result<()> {
		// MySQL spells the shared lock `LOCK IN SHARE MODE` (no `FOR SHARE` keyword).
		writer.write_all(match lock {
			squealy::RowLock::Update => b" FOR UPDATE",
			squealy::RowLock::Share => b" LOCK IN SHARE MODE",
		})
	}

	fn extract_second_uses_microsecond_unit(&self) -> bool {
		// MySQL's `EXTRACT(SECOND …)` is integer-only; use the composite `SECOND_MICROSECOND` unit to
		// recover the fractional part.
		true
	}
}

fn write_delimited(value: &str, delimiter: char, writer: &mut impl Write) -> io::Result<()> {
	let mut encoded = [0u8; 4];
	let delim = delimiter.encode_utf8(&mut encoded).as_bytes();
	writer.write_all(delim)?;
	let mut start = 0;
	for (index, _) in value.match_indices(delimiter) {
		writer.write_all(&value.as_bytes()[start..index])?;
		writer.write_all(delim)?;
		writer.write_all(delim)?;
		start = index + delimiter.len_utf8();
	}
	writer.write_all(&value.as_bytes()[start..])?;
	writer.write_all(delim)
}
use std::io::{self, Write};

use squealy::SqlType;
