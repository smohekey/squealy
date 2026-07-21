/// PostgreSQL's [`Dialect`]: positional `$n` placeholders, `"`-quoted identifiers, and `double
/// precision` casts. The query renderer routes its dialect-specific output through this so the sink
/// logic can be shared (see [`squealy::Dialect`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PostgresDialect;

impl squealy::Dialect for PostgresDialect {
	fn write_placeholder(&self, index: usize, writer: &mut dyn Write) -> io::Result<()> {
		// PostgreSQL parameters are 1-based and positional.
		write!(writer, "${}", index + 1)
	}

	fn write_quoted_ident(&self, ident: &str, mut writer: &mut dyn Write) -> io::Result<()> {
		write_quoted_ident(ident, &mut writer)
	}

	fn write_cast_type(&self, ty: &SqlType, mut writer: &mut dyn Write) -> io::Result<()> {
		if let SqlType::Enum(name) = ty {
			// A cast to an enum type inside an expression (a check/generated/index/view body) has no
			// schema context here, so it would render an unqualified `AS "mood"` that fails to resolve
			// when the enum's schema is off `search_path`. Reject rather than emit unresolvable SQL.
			return Err(io::Error::new(
				io::ErrorKind::Unsupported,
				format!("squealy does not support casting to the enum type `{name}` in an expression"),
			));
		}
		write_pg_sql_type(ty, &mut writer)
	}

	fn write_like_operator(
		&self,
		case_insensitive: bool,
		negated: bool,
		writer: &mut dyn Write,
	) -> io::Result<()> {
		// PostgreSQL has a native case-insensitive `ILIKE`.
		writer.write_all(match (case_insensitive, negated) {
			(false, false) => b" LIKE " as &[u8],
			(false, true) => b" NOT LIKE ",
			(true, false) => b" ILIKE ",
			(true, true) => b" NOT ILIKE ",
		})
	}

	fn concat_uses_pipe_operator(&self) -> bool {
		// `a || b` propagates NULL (matching the builder's nullability model) and lets a bare
		// parameter's type be inferred, unlike PostgreSQL's NULL-ignoring `CONCAT("any", …)`.
		true
	}

	fn substring_bounds_need_cast(&self) -> bool {
		// Cast `start`/`len` to integer so a bare parameter is the positional count, not the regex
		// `substring(text FROM pattern FOR escape)` overload.
		true
	}

	fn timestamp_operand_needs_cast(&self) -> bool {
		// Cast a bare literal/param operand of EXTRACT/date_trunc to its timestamp type — both are
		// overloaded, so an untyped placeholder can't be resolved when preparing the statement.
		true
	}
}

fn write_pg_sql_type(ty: &SqlType, writer: &mut impl Write) -> io::Result<()> {
	let name = match ty {
		SqlType::Bool => "boolean",
		SqlType::I8 | SqlType::I16 => "smallint",
		SqlType::I32 => "integer",
		SqlType::I64 | SqlType::Isize => "bigint",
		SqlType::I128 => "numeric",
		SqlType::U8 => "smallint",
		SqlType::U16 => "integer",
		SqlType::U32 | SqlType::Usize => "bigint",
		SqlType::U64 | SqlType::U128 => "numeric",
		SqlType::F32 => "real",
		SqlType::F64 => "double precision",
		SqlType::String | SqlType::Text => "text",
		SqlType::Varchar(length) => return write!(writer, "varchar({length})"),
		SqlType::Char(length) => return write!(writer, "char({length})"),
		SqlType::Decimal { precision, scale } => {
			return write!(writer, "numeric({precision},{scale})");
		}
		SqlType::Date => "date",
		// `time(n)` / `timestamp(n) [with time zone]` — the fractional-seconds precision goes between the
		// base name and the `with time zone` suffix. `None` renders the bare form (PostgreSQL then uses
		// its microsecond default, which introspection reads back as `Some(6)`).
		SqlType::Time { tz, precision } => {
			return write_pg_temporal(writer, "time", *tz, *precision);
		}
		SqlType::Timestamp { tz, precision } => {
			return write_pg_temporal(writer, "timestamp", *tz, *precision);
		}
		SqlType::Uuid => "uuid",
		SqlType::Json => "json",
		SqlType::Jsonb => "jsonb",
		SqlType::Bytes => "bytea",
		// PostgreSQL has no fixed-length binary type.
		SqlType::FixedBytes(_) => "bytea",
		// A user-defined enum type is referenced by its quoted name.
		SqlType::Enum(name) => return write_quoted_ident(name, writer),
		SqlType::Raw(raw) => raw.as_str(),
	};
	writer.write_all(name.as_bytes())
}

/// Renders a `time`/`timestamp` type with its optional fractional-seconds precision and the
/// `with time zone` suffix. PostgreSQL spells the precision *inside* the base name (`timestamp(3) with
/// time zone`), so it cannot be a trailing modifier.
fn write_pg_temporal(
	writer: &mut impl Write,
	base: &str,
	tz: bool,
	precision: Option<u8>,
) -> io::Result<()> {
	writer.write_all(base.as_bytes())?;
	if let Some(precision) = precision {
		write!(writer, "({precision})")?;
	}
	if tz {
		writer.write_all(b" with time zone")?;
	}
	Ok(())
}

fn write_quoted(value: &str, delimiter: char, writer: &mut impl Write) -> io::Result<()> {
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

/// Writes a single SQL identifier wrapped in double quotes, doubling any embedded
/// quotes. This keeps reserved words (`user`, `order`, ...) and identifiers with
/// special characters valid. Identifiers come from compile-time table metadata, so
/// this is robustness, not injection defense.
fn write_quoted_ident(value: &str, writer: &mut impl Write) -> io::Result<()> {
	write_quoted(value, '"', writer)
}

/// Writes a schema-qualified table reference with each part quoted separately,
/// e.g. `"public"."users"`.
use std::io::{self, Write};

use squealy::SqlType;
