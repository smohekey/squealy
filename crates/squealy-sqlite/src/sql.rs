use std::io::{self, Write};

use squealy::SqlType;

/// The SQLite type affinity for a neutral [`SqlType`]. SQLite is dynamically typed, so the column type
/// only assigns one of five affinities.
pub(crate) fn sqlite_affinity(ty: &SqlType) -> &str {
	match ty {
		SqlType::Bool
		| SqlType::I8
		| SqlType::I16
		| SqlType::I32
		| SqlType::I64
		| SqlType::I128
		| SqlType::Isize
		| SqlType::U8
		| SqlType::U16
		| SqlType::U32
		| SqlType::U64
		| SqlType::U128
		| SqlType::Usize => "INTEGER",
		SqlType::F32 | SqlType::F64 => "REAL",
		SqlType::Decimal { .. } => "NUMERIC",
		SqlType::String
		| SqlType::Varchar(_)
		| SqlType::Char(_)
		| SqlType::Text
		| SqlType::Date
		| SqlType::Time { .. }
		| SqlType::Timestamp { .. }
		| SqlType::Uuid
		| SqlType::Json
		| SqlType::Jsonb => "TEXT",
		SqlType::Bytes | SqlType::FixedBytes(_) => "BLOB",
		// Enum casts are rejected before affinity lookup; keep the mapping exhaustive.
		SqlType::Enum(_) => "TEXT",
		SqlType::Raw(raw) => raw.as_str(),
	}
}

/// Quotes an identifier with double quotes, doubling any embedded double quote.
fn write_quoted_ident(value: &str, writer: &mut impl Write) -> io::Result<()> {
	write_delimited(value, '"', writer)
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

/// SQLite's [`Dialect`](squealy::Dialect): `?` placeholders, double-quoted identifiers, and SQLite
/// `CAST` affinity names, plus the SQLite spellings for the seams other backends default
/// differently — schema suppression (`qualify_schema`), `length`/`substr`/`||` builtins, `RETURNING`
/// without a target alias, and the `SELECT * FROM (…)` set-operand wrapper. Everything else uses the
/// trait defaults, which already match SQLite (integer-division float cast, `DEFAULT VALUES` empty
/// inserts, `NULLS FIRST`/`LAST`, `ON CONFLICT` upserts, `UPDATE … FROM`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SqliteDialect;

impl squealy::Dialect for SqliteDialect {
	fn write_placeholder(&self, _index: usize, writer: &mut dyn Write) -> io::Result<()> {
		// SQLite uses positional `?` placeholders.
		writer.write_all(b"?")
	}

	fn write_quoted_ident(&self, ident: &str, mut writer: &mut dyn Write) -> io::Result<()> {
		write_quoted_ident(ident, &mut writer)
	}

	fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
		// A user-defined enum is a PostgreSQL-only type. SQLite has no equivalent, and `sqlite_affinity`
		// would silently collapse it to `TEXT` — reject it instead (the enum column class is already
		// refused up front; this catches an enum buried in a cast expression).
		if let SqlType::Enum(name) = ty {
			return Err(io::Error::new(
				io::ErrorKind::Unsupported,
				format!("SQLite cannot render a CAST to the user-defined enum type `{name}`"),
			));
		}
		// `CAST(expr AS <type>)` uses SQLite's affinity names, the same mapping as the column type.
		writer.write_all(sqlite_affinity(ty).as_bytes())
	}

	fn write_general_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
		squealy::reject_128bit_general_cast(ty)?;
		// SQLite's `CAST` uses affinity names only (`NUMERIC`), which drop a decimal's precision and scale,
		// so a general `CAST(x AS DECIMAL(10, 2))` cannot be rendered faithfully; reject it rather than
		// silently narrow it (the reverse parser's `general_cast` keeps SQLite `Decimal` casts `Raw` for the
		// same reason, so a structural one could only arrive via a cross-dialect deploy). See 8fe1530.
		if matches!(ty, SqlType::Decimal { .. }) {
			return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SQLite cannot render a general CAST to DECIMAL/NUMERIC faithfully (its NUMERIC affinity \
                 drops precision and scale); author it as a Raw expression if a SQLite spelling is intended",
            ));
		}
		self.write_cast_type(ty, writer)
	}

	fn unary_string_fn_name(&self, func: squealy::UnaryStringFunc) -> &'static str {
		match func {
			// SQLite has no `CHAR_LENGTH`; `length()` counts characters for TEXT values.
			squealy::UnaryStringFunc::Length => "length",
			other => other.sql_name(),
		}
	}

	fn qualify_schema(&self) -> bool {
		// SQLite has no schemas; table names render unqualified (matching the flattened DDL).
		false
	}

	fn returning_omits_target_alias(&self) -> bool {
		// SQLite's UPDATE/DELETE `RETURNING` cannot resolve the target-table alias (`no such column:
		// q0_0.col`); a single-table statement is unambiguous, so the columns render bare.
		true
	}

	fn set_operand_style(&self) -> squealy::SetOperandStyle {
		// SQLite rejects a parenthesized compound operand and a per-operand `ORDER BY`/`LIMIT`, so an
		// operand is wrapped as `SELECT * FROM (SELECT …)` (valid for ordered/limited/nested operands).
		squealy::SetOperandStyle::SubquerySelect
	}

	fn supports_intersect_except_all(&self) -> bool {
		// SQLite allows `ALL` only after `UNION`; `INTERSECT ALL`/`EXCEPT ALL` are syntax errors.
		false
	}

	fn substring_uses_function_call(&self) -> bool {
		// SQLite spells substring as `substr(s, start, len)`, not `SUBSTRING(s FROM start FOR len)`.
		true
	}

	fn supports_parenthesized_recursive_cte_arm(&self) -> bool {
		// SQLite's recursive-CTE grammar rejects any parenthesized recursive arm, so an arm carrying its
		// own ORDER BY/LIMIT/OFFSET (which needs parens to scope) has no valid rendering and is rejected.
		false
	}

	fn concat_uses_pipe_operator(&self) -> bool {
		// SQLite has no null-propagating `CONCAT`; `||` returns NULL if either operand is NULL,
		// matching squealy's concat expression (nullable iff either operand is nullable).
		true
	}

	fn delete_using_style(&self) -> squealy::DeleteUsingStyle {
		// SQLite has no join-delete; a correlated delete becomes `DELETE … WHERE EXISTS (SELECT …)`.
		squealy::DeleteUsingStyle::SqliteExists
	}
}
