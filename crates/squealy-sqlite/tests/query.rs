//! SQLite query rendering through the shared core renderer + `SqliteDialect`.
//!
//! This slice is driver-free, so the tests only assert `to_sql()` / `collect_params()` output — no
//! execution. They cover the reachable render paths: a filtered `SELECT`, an `INSERT` (via the upsert
//! `build()` inspection path, since SQLite advertises no `RETURNING` in this slice), a correlated
//! `UPDATE … FROM`, a correlated `DELETE … USING`, a `UNION` set operation (with sub-select-wrapped
//! operands, including an ordered operand), and the SQLite-specific spellings of the character-length
//! (`length()`), substring (`substr()`), and string-concatenation (`||`) scalars.

use squealy::*;
use squealy_sqlite::{Sqlite, SqliteError, SqliteValue};

#[derive(Clone, Debug, PartialEq, Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	name: C::Type<'scope, String>,
	count: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Gadget<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	widget_id: C::Type<'scope, i32>,
}

#[test]
fn sqlite_renders_select_with_where_in_its_dialect() {
	let query = Sqlite
		.from::<Widget>()
		.where_(|widget| widget.name.equals("Ada"))
		.select(|(widget,)| (widget.id, widget.name));

	let sql = query.to_sql();

	// Double-quoted identifiers and a positional `?` placeholder (not Postgres `$1`, not MySQL backticks).
	assert!(
		sql.contains("\"widgets\""),
		"expected double-quoted table: {sql}"
	);
	assert!(sql.contains('?'), "expected a `?` placeholder: {sql}");
	assert!(
		!sql.contains("$1"),
		"must not use Postgres placeholders: {sql}"
	);
	assert!(!sql.contains('`'), "must not use MySQL backticks: {sql}");

	assert_eq!(
		query.collect_params().unwrap(),
		vec![SqliteValue::Text("Ada".to_owned())]
	);
}

#[test]
fn sqlite_renders_insert() {
	// A plain insert is only inspectable (driver-free, no `RETURNING`) through the upsert `build()`
	// path; `do_nothing()` with no conflict column keeps the `INSERT` otherwise plain.
	let insert = Sqlite
		.to::<Widget>()
		.name("Ada")
		.count(0)
		.on_conflict(|widget| widget.id)
		.do_nothing()
		.build();

	let sql = insert.to_sql();
	assert!(
		sql.starts_with("INSERT INTO \"widgets\" (\"name\", \"count\") VALUES (?, ?)"),
		"{sql}"
	);
	assert!(
		sql.contains("ON CONFLICT (\"id\") DO NOTHING"),
		"expected an ON CONFLICT clause: {sql}"
	);
	assert_eq!(
		insert.collect_params().unwrap(),
		vec![SqliteValue::Text("Ada".to_owned()), SqliteValue::Integer(0)]
	);
}

#[test]
fn sqlite_renders_correlated_update() {
	// SQLite renders a correlated update as `UPDATE t AS a SET … FROM other AS b WHERE <correlation>`.
	let update = Sqlite
		.to_columns::<Widget, (WidgetCount,)>()
		.from::<Gadget>()
		.set(|(_widget, gadget)| (gadget.widget_id,))
		.where_(|(widget, gadget)| widget.id.equals(gadget.id))
		.build();

	let sql = update.to_sql();
	assert!(sql.starts_with("UPDATE \"widgets\" AS "), "{sql}");
	assert!(sql.contains("SET \"count\" = "), "{sql}");
	assert!(sql.contains("FROM \"gadgets\" AS "), "{sql}");
	assert!(!sql.contains('`'), "must not use MySQL backticks: {sql}");
	assert_eq!(update.collect_params().unwrap(), Vec::<SqliteValue>::new());
}

#[test]
fn sqlite_renders_correlated_delete() {
	// SQLite renders a correlated delete as `DELETE FROM t AS a USING other AS b WHERE <correlation>`.
	let delete = Sqlite
		.from::<Widget>()
		.using::<Gadget>()
		.where_(|(widget, gadget)| widget.id.equals(gadget.widget_id))
		.build();

	let sql = delete.to_sql();
	// SQLite has no join-delete, so a correlated delete is a correlated EXISTS subquery.
	assert!(sql.starts_with("DELETE FROM \"widgets\" AS "), "{sql}");
	assert!(
		sql.contains("WHERE EXISTS (SELECT 1 FROM \"gadgets\" AS "),
		"expected an EXISTS-subquery correlated delete: {sql}"
	);
	assert!(
		!sql.contains("USING"),
		"SQLite has no DELETE … USING: {sql}"
	);
	assert!(!sql.contains('`'), "must not use MySQL backticks: {sql}");
	assert_eq!(delete.collect_params().unwrap(), Vec::<SqliteValue>::new());
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	handle: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
	accounts: Account<'static, ColumnName>,
}

#[test]
fn sqlite_suppresses_schema_qualification() {
	// A `#[schema(Public)]` table renders unqualified for SQLite (which has no schemas), matching how
	// its DDL flattens schemas — not `"public"."accounts"`, which SQLite would read as a database name.
	let query = Sqlite.from::<Account>().select(|(account,)| account.id);
	let sql = query.to_sql();
	assert!(sql.contains("FROM \"accounts\""), "{sql}");
	assert!(
		!sql.contains("\"public\""),
		"must not qualify with the schema: {sql}"
	);
}

#[test]
fn sqlite_substring_renders_as_substr_function_call() {
	// SQLite has no `SUBSTRING(s FROM start FOR len)`; it uses the `substr(s, start, len)` call.
	let query = Sqlite
		.from::<Widget>()
		.select(|(widget,)| substring(widget.name, 2, 3));
	let sql = query.to_sql();
	assert!(sql.contains("substr("), "expected substr(...): {sql}");
	assert!(
		!sql.contains("SUBSTRING"),
		"SQLite must not use SUBSTRING: {sql}"
	);
	// The `FROM start FOR len` substring syntax is gone (the ` FOR ` keyword is unique to it).
	assert!(
		!sql.contains(" FOR "),
		"no FROM/FOR substring syntax: {sql}"
	);
}

#[test]
fn sqlite_renders_union_set_operation() {
	let union = Sqlite
		.from::<Widget>()
		.select(|(widget,)| (widget.id, widget.name))
		.union(
			Sqlite
				.from::<Widget>()
				.where_(|widget| widget.name.equals("Ada"))
				.select(|(widget,)| (widget.id, widget.name)),
		);

	let sql = union.to_sql();
	// SQLite rejects a parenthesized compound operand `(SELECT …)`, so each operand is wrapped as a
	// sub-select `SELECT * FROM (SELECT …)` — which also keeps ordered/limited operands valid.
	assert!(
		sql.starts_with("SELECT * FROM (SELECT "),
		"expected a sub-select-wrapped operand: {sql}"
	);
	assert!(
		sql.contains(") UNION SELECT * FROM (SELECT "),
		"expected sub-select-wrapped UNION operands: {sql}"
	);
	assert!(
		!sql.contains(") UNION ("),
		"SQLite must not parenthesize set operands: {sql}"
	);
	assert!(
		sql.contains("\"widgets\""),
		"expected double-quoted identifiers: {sql}"
	);
	assert!(!sql.contains('`'), "must not use MySQL backticks: {sql}");
	assert_eq!(
		union.collect_params().unwrap(),
		vec![SqliteValue::Text("Ada".to_owned())]
	);
}

#[test]
fn sqlite_wraps_ordered_set_operand_as_subquery() {
	// An operand with its own ORDER BY/LIMIT can't sit bare in a SQLite compound (SQLite only allows a
	// trailing ORDER BY/LIMIT on the whole compound), so it must render as `SELECT * FROM (SELECT … ORDER
	// BY … LIMIT …)`.
	let union = Sqlite
		.from::<Widget>()
		.order_by(|(widget,)| widget.id.asc())
		.limit(1)
		.select(|(widget,)| (widget.id, widget.name))
		.union(
			Sqlite
				.from::<Widget>()
				.select(|(widget,)| (widget.id, widget.name)),
		);

	let sql = union.to_sql();
	assert!(
		sql.starts_with("SELECT * FROM (SELECT "),
		"ordered operand must be sub-select wrapped: {sql}"
	);
	assert!(
		sql.contains("ORDER BY ") && sql.contains("LIMIT 1)"),
		"the operand's ORDER BY/LIMIT must stay inside its sub-select: {sql}"
	);
	// The compound-level `UNION` must follow the closed sub-select, not a bare `ORDER BY … UNION`.
	assert!(
		sql.contains("LIMIT 1) UNION SELECT * FROM (SELECT "),
		"expected the ORDER BY/LIMIT to bind to the operand, not the compound: {sql}"
	);
}

#[test]
fn sqlite_wraps_nested_set_group_operand_as_subquery() {
	// A completed set-select used as another operand (`a.union(b).union(c)`) becomes a `SetGroup`, which
	// must also use SQLite's sub-select wrapping — not `(a UNION b) UNION c`, which SQLite rejects with a
	// `near "(": syntax error`.
	let union = Sqlite
		.from::<Widget>()
		.select(|(widget,)| (widget.id, widget.name))
		.union(
			Sqlite
				.from::<Widget>()
				.select(|(widget,)| (widget.id, widget.name)),
		)
		.union(
			Sqlite
				.from::<Widget>()
				.select(|(widget,)| (widget.id, widget.name)),
		);

	let sql = union.to_sql();
	// The whole statement must not open with a parenthesized compound.
	assert!(
		!sql.starts_with('('),
		"SQLite must not open with a parenthesized compound operand: {sql}"
	);
	// The nested `a UNION b` group renders as a sub-select, then the outer `UNION c`.
	assert!(
		sql.starts_with("SELECT * FROM (SELECT * FROM (SELECT "),
		"expected the nested group to be sub-select wrapped: {sql}"
	);
	assert!(
		!sql.contains(") UNION ("),
		"SQLite must not parenthesize any set operand: {sql}"
	);
}

#[test]
fn sqlite_concat_renders_as_pipe_operator() {
	// SQLite's `CONCAT` ignores NULL operands, but squealy's concat is nullable iff either operand is;
	// the null-propagating `||` operator matches that, so the dialect must render `||`, not `CONCAT`.
	let query = Sqlite
		.from::<Widget>()
		.select(|(widget,)| widget.name.concat(widget.name));

	let sql = query.to_sql();
	assert!(sql.contains(" || "), "expected the `||` operator: {sql}");
	assert!(
		!sql.contains("CONCAT"),
		"SQLite must not render CONCAT: {sql}"
	);
}

#[test]
fn sqlite_length_renders_as_length_not_char_length() {
	// SQLite has no `CHAR_LENGTH`; the dialect maps the character-length scalar to `length(...)`.
	let query = Sqlite
		.from::<Widget>()
		.select(|(widget,)| length(widget.name));

	let sql = query.to_sql();
	assert!(
		sql.contains("length(") && sql.contains("\"name\")"),
		"expected length(…\"name\"): {sql}"
	);
	assert!(
		!sql.contains("CHAR_LENGTH"),
		"SQLite must not render CHAR_LENGTH: {sql}"
	);
}

// --- Fallible render: a scoped recursive CTE arm has no valid SQLite rendering (git-bug 1e67ff8) ---

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Warehouse)]
struct Node<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	parent_id: C::Type<'scope, Option<i32>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Warehouse {
	nodes: Node<'static, ColumnName>,
}

// A recursive CTE whose anchor carries its own ORDER BY/LIMIT — a *scoped* arm. It can only be
// scoped by parenthesizing it, which SQLite's recursive-CTE grammar forbids: no valid rendering.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, RecursiveCTE)]
struct BoundedAncestor<'scope, C: ColumnMode = ColumnExpr> {
	id: C::Type<'scope, i32>,
	depth: C::Type<'scope, i32>,
}

impl<'scope, C: ColumnMode> RecursiveCteDefinition for BoundedAncestor<'scope, C> {
	const UNION_ALL: bool = true;

	fn definition(
		db: &'static ModelConn,
		recur: RecursiveSelf<'static, Self>,
	) -> impl RecursiveBody<Row = <Self as SchemaCte>::Row> {
		let anchor = db
			.from::<Node>()
			.where_(|node| node.parent_id.is_null())
			.order_by(|(node,)| node.id.asc())
			.limit(5)
			.project(|(node,)| (node.id, 0));
		let step = recur
			.from()
			.join::<Node>()
			.on(|(ancestor,), node| node.parent_id.equals(ancestor.id))
			.project(|(ancestor, node)| (node.id, ancestor.depth + 1));
		anchor.union_with(step)
	}
}

#[test]
fn sqlite_scoped_recursive_arm_try_to_sql_returns_render_error() {
	// The fallible render surfaces the reject as a returned error rather than panicking.
	let query = Sqlite
		.from::<BoundedAncestor>()
		.select(|(ancestor,)| (ancestor.id, ancestor.depth));
	let error = query
		.try_to_sql()
		.expect_err("SQLite cannot render a scoped recursive CTE arm");
	assert!(
		matches!(error, SqliteError::Render(_)),
		"expected a render error, got {error:?}"
	);
}

#[test]
fn sqlite_scoped_recursive_arm_collect_params_returns_render_error() {
	// The parameter collector routes the same reject through the fallible path (it renders the WITH
	// prefix, whose scoped arm cannot render), so it too returns an error instead of panicking.
	let query = Sqlite
		.from::<BoundedAncestor>()
		.select(|(ancestor,)| (ancestor.id, ancestor.depth));
	let error = query
		.collect_params()
		.expect_err("SQLite cannot render a scoped recursive CTE arm");
	assert!(
		matches!(error, SqliteError::Render(_)),
		"expected a render error, got {error:?}"
	);
}

#[test]
#[should_panic(expected = "render SQL")]
fn sqlite_scoped_recursive_arm_to_sql_still_panics() {
	// The infallible `to_sql()` still panics on an unrenderable shape (documented behaviour); callers
	// that build dynamic CTE queries use `try_to_sql()` to recover the error instead.
	let query = Sqlite
		.from::<BoundedAncestor>()
		.select(|(ancestor,)| (ancestor.id, ancestor.depth));
	let _ = query.to_sql();
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Lineage<'scope, C: ColumnMode = ColumnExpr> {
	node_id: C::Type<'scope, i32>,
	depth: C::Type<'scope, i32>,
}

#[test]
fn sqlite_scoped_recursive_arm_insert_select_collect_params_returns_render_error() {
	// `INSERT … SELECT` whose source references the unrenderable recursive CTE: the params collector
	// must surface the render reject too (not swallow it and return `Ok`), matching select/set params.
	let query = Sqlite.to::<Lineage>().insert_select(
		|lineage| (lineage.node_id, lineage.depth),
		Sqlite
			.from::<BoundedAncestor>()
			.select(|(ancestor,)| (ancestor.id, ancestor.depth)),
	);
	let error = query
		.collect_params()
		.expect_err("SQLite cannot render a scoped recursive CTE arm in the source");
	assert!(
		matches!(error, SqliteError::Render(_)),
		"expected a render error, got {error:?}"
	);
}
