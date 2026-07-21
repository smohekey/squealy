use std::io::{self, Write};

use crate::{OrderNulls, RowLock, SqlType, UnaryStringFunc};

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
	///
	/// This is the spelling for a **result-pin** cast — a view/query output whose exact type is recovered
	/// from the introspected output column, so a many-to-one representative (bare `DECIMAL`, `SIGNED`) is
	/// correct. A **general** authored cast renders through
	/// [`write_general_cast_type`](Self::write_general_cast_type) instead, which must be faithful.
	fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()>;

	/// Writes the type name for a **general** `CAST(expr AS <type>)` — an authored
	/// [`ExprNode::Cast`](crate::ExprNode::Cast) whose precision/scale *is* the semantics, as opposed to a
	/// result-pin cast (which pins a wire type and recovers the exact type from the output column, so a bare
	/// `DECIMAL`/`NUMERIC` is fine there). A general cast must render *faithfully*: a backend that cannot
	/// spell the target type exactly rejects it here rather than silently narrow it (which would change the
	/// deployed semantics and churn every re-plan). The default rejects only the universally un-renderable
	/// 128-bit-integer cast (see [`reject_128bit_general_cast`]) and otherwise defers to
	/// [`write_cast_type`](Self::write_cast_type), which is exact on PostgreSQL (`numeric(p, s)`). MySQL
	/// overrides it to spell `DECIMAL(p, s)` faithfully; SQLite overrides it to reject a `Decimal` cast (its
	/// `NUMERIC` affinity drops precision/scale). See git-bug 8fe1530.
	fn write_general_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
		reject_128bit_general_cast(ty)?;
		self.write_cast_type(ty, writer)
	}

	/// Whether `/` performs integer division when both operands are integers, so the renderer must
	/// cast operands to floating point to get the query builder's always-fractional division.
	///
	/// PostgreSQL does integer division (so this is `true`); MySQL's `/` is already floating-point
	/// division (`false`), and casting would change `DECIMAL` results.
	fn integer_division_needs_float_cast(&self) -> bool {
		true
	}

	/// The fractional-seconds precision to render on `CURRENT_TIMESTAMP` for a `now()` expression, or
	/// `None` to render it bare.
	///
	/// The `now()` value types are microsecond-resolution, and native timestamp columns are
	/// `TIMESTAMP(6)`; MySQL's bare `CURRENT_TIMESTAMP` is fsp 0, so a value produced by `now()` would
	/// lose its sub-seconds unless spelled `CURRENT_TIMESTAMP(6)`. The default is `None` — PostgreSQL's
	/// `CURRENT_TIMESTAMP` is already microsecond.
	fn now_fractional_digits(&self) -> Option<u8> {
		None
	}

	/// The SQL name for a scalar string function. The default is the standard spelling
	/// ([`UnaryStringFunc::sql_name`], shared by PostgreSQL and MySQL); a backend overrides it where its
	/// builtin differs — e.g. SQLite spells character length `length` rather than `CHAR_LENGTH`.
	fn unary_string_fn_name(&self, func: UnaryStringFunc) -> &'static str {
		func.sql_name()
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

	/// How `UPDATE … FROM` / `DELETE … USING` render a correlated extra source. PostgreSQL appends the
	/// source after the `SET`/target with the correlation in `WHERE`; MySQL joins the source before the
	/// `SET`, with the correlation in the join's `ON`. Defaults to the PostgreSQL form.
	fn update_from_style(&self) -> UpdateFromStyle {
		UpdateFromStyle::PgFrom
	}

	/// How a correlated `DELETE … <source>` renders. The default derives from
	/// [`update_from_style`](Self::update_from_style), so PostgreSQL/MySQL are unchanged; SQLite
	/// overrides it, having no join-delete syntax — it rewrites the correlated delete as
	/// `DELETE FROM t AS a WHERE EXISTS (SELECT 1 FROM other AS b WHERE <correlation>)`.
	fn delete_using_style(&self) -> DeleteUsingStyle {
		match self.update_from_style() {
			UpdateFromStyle::PgFrom => DeleteUsingStyle::PgUsing,
			UpdateFromStyle::MysqlJoin => DeleteUsingStyle::MysqlJoin,
		}
	}

	/// Whether a schema/namespace qualifier is emitted before a table name. Defaults to `true`; SQLite
	/// has no schemas (tables render unqualified/flattened), so it returns `false`.
	fn qualify_schema(&self) -> bool {
		true
	}

	/// Whether an `UPDATE`/`DELETE … RETURNING` clause references columns *unqualified* (bare column
	/// names) rather than qualified by the statement's target-table alias.
	///
	/// Defaults to `false`: PostgreSQL aliases the target (`UPDATE t AS q0_0 … RETURNING q0_0.col`) and
	/// resolves the alias in `RETURNING`. SQLite also aliases the target but its `RETURNING` clause
	/// cannot resolve that alias (`no such column: q0_0.col`), so it returns `true` and — since an
	/// `UPDATE`/`DELETE` targets a single table, leaving the columns unambiguous — renders them bare.
	/// (An `INSERT … RETURNING` has no alias and is always unqualified, independently of this.)
	fn returning_omits_target_alias(&self) -> bool {
		false
	}

	/// How the operands of a set operation (`UNION`/`INTERSECT`/`EXCEPT`) are wrapped. Defaults to
	/// [`SetOperandStyle::Parenthesized`] (`(SELECT …)`); SQLite rejects a parenthesized compound
	/// operand *and* a per-operand `ORDER BY`/`LIMIT`, so it uses [`SetOperandStyle::SubquerySelect`]
	/// (`SELECT * FROM (SELECT …)`), which stays valid for ordered/limited operands and preserves the
	/// grouping of a nested compound.
	fn set_operand_style(&self) -> SetOperandStyle {
		SetOperandStyle::Parenthesized
	}

	/// Whether the dialect supports the `ALL` quantifier on `INTERSECT`/`EXCEPT` (`INTERSECT ALL` /
	/// `EXCEPT ALL`) in a view body. Defaults to `true`; SQLite allows `ALL` only after `UNION`, so it
	/// returns `false` and the view renderer rejects an `INTERSECT ALL`/`EXCEPT ALL` set-op body for it
	/// (mirroring the runtime query API's `SupportsIntersectExceptAll` gate, which SQLite also lacks).
	fn supports_intersect_except_all(&self) -> bool {
		true
	}

	/// Whether `substring` renders as the comma-argument call `substr(s, start, len)` rather than the
	/// SQL-standard `SUBSTRING(s FROM start FOR len)`. Defaults to `false`; SQLite has no `FROM`/`FOR`
	/// substring syntax, so it returns `true`.
	fn substring_uses_function_call(&self) -> bool {
		false
	}

	/// Whether a recursive-CTE arm (`WITH RECURSIVE t AS (<anchor> UNION [ALL] <recursive>)`) may be
	/// **parenthesized**. A plain, tail-less arm always renders bare; but an arm carrying its own
	/// `ORDER BY`/`LIMIT`/`OFFSET` (or a nested compound) can only be scoped by wrapping it in `(…)`.
	/// Defaults to `true` (PostgreSQL/MySQL accept `(SELECT … LIMIT n) UNION ALL …`); SQLite's
	/// recursive-CTE grammar rejects *any* parenthesized recursive arm, so it returns `false` and such an
	/// arm is rejected there (it has no valid rendering). Distinct from [`set_operand_style`](Self::set_operand_style):
	/// that governs a plain compound's operands, this governs a *recursive-CTE* body's arms, where SQLite
	/// forbids even the `SELECT * FROM (…)` sub-select wrapping.
	fn supports_parenthesized_recursive_cte_arm(&self) -> bool {
		true
	}
}

/// Rejects a **general** `CAST(x AS ty)` whose target type no backend can render faithfully: a 128-bit
/// integer ([`SqlType::I128`]/[`SqlType::U128`]). No dialect spells a native 128-bit-integer cast —
/// PostgreSQL renders bare `numeric`, MySQL `DECIMAL(65, 0)`, SQLite `INTEGER` — and each of those
/// re-introspects to a *different* type (the reverse parser's `general_cast` guard keeps it `Raw`), so a
/// structural `Cast { ty: I128 | U128 }` cannot round-trip on any backend. Rendering rejects it rather than
/// emit a lossy, non-round-tripping cast; author it as a [`ExprNode::Raw`](crate::ExprNode::Raw) if a
/// specific dialect spelling is intended. Shared by every backend's
/// [`Dialect::write_general_cast_type`]. See git-bug 8fe1530.
pub fn reject_128bit_general_cast(ty: &SqlType) -> io::Result<()> {
	if matches!(ty, SqlType::I128 | SqlType::U128) {
		return Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"a general CAST to a 128-bit integer type cannot be rendered faithfully on any backend \
             (no dialect has a native 128-bit-integer cast); author it as a Raw expression if a \
             specific dialect spelling is intended",
		));
	}
	Ok(())
}

/// The two shapes a correlated `UPDATE … <source>` / `DELETE … <source>` takes across dialects (see
/// [`Dialect::update_from_style`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateFromStyle {
	/// PostgreSQL: `UPDATE t AS a SET … FROM other AS b WHERE <correlation [AND filters]>` and
	/// `DELETE FROM t AS a USING other AS b WHERE <correlation [AND filters]>`.
	PgFrom,
	/// MySQL: `UPDATE t AS a JOIN other AS b ON <correlation> SET … [WHERE filters]` and
	/// `DELETE a FROM t AS a JOIN other AS b ON <correlation> [WHERE filters]`.
	MysqlJoin,
}

/// The shapes a correlated `DELETE … <source>` takes across dialects (see
/// [`Dialect::delete_using_style`]). Decoupled from [`UpdateFromStyle`] because SQLite can render
/// `UPDATE … FROM` but has no join-delete, so it falls back to a correlated `EXISTS` subquery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteUsingStyle {
	/// PostgreSQL: `DELETE FROM t AS a USING other AS b WHERE <correlation>`.
	PgUsing,
	/// MySQL: `DELETE a FROM t AS a JOIN other AS b ON <correlation>`.
	MysqlJoin,
	/// SQLite: `DELETE FROM t AS a WHERE EXISTS (SELECT 1 FROM other AS b WHERE <correlation>)`.
	SqliteExists,
}

/// How a set-operation operand is wrapped when rendered (see [`Dialect::set_operand_style`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOperandStyle {
	/// PostgreSQL/MySQL: `(SELECT …)` — each operand parenthesized.
	Parenthesized,
	/// SQLite: `SELECT * FROM (SELECT …)` — each operand wrapped as a sub-select. SQLite's
	/// compound-select grammar rejects a parenthesized operand and a bare operand's `ORDER BY`/`LIMIT`,
	/// so the sub-select form is the only one that stays valid for ordered/limited operands and that
	/// preserves the grouping of a nested compound.
	SubquerySelect,
}
