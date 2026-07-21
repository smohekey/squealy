//! Deterministic create-from-scratch SQLite DDL for Squealy database models.
//!
//! This crate only renders SQL. Execute the result with
//! [`SqliteConnection::execute_batch`](https://docs.rs/squealy-sqlite/latest/squealy_sqlite/struct.SqliteConnection.html#method.execute_batch)
//! or another SQLite driver. It does not inspect, diff, migrate, or version existing schemas.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{self, Write};

use squealy_core::{
	render_scalar_expr, CheckModel, ColumnModel, Constraint, ConstraintDeferrability, DatabaseModel,
	DefaultValue, Dialect, ForeignKeyAction, ForeignKeyMatch, ForeignKeyModel, IndexModel, SqlType,
	TableModel,
};

/// Renders create-from-scratch SQLite DDL for `model`.
///
/// Schemas are flattened into SQLite's single database-wide object namespace. Statements retain
/// model order: every table is emitted first, followed by every index. Statements are separated by
/// `";\n"`, and a non-empty result always ends in `;`.
///
/// Output is byte-identical for the same model and this renderer version. Formatting may change in a
/// future release, so a persisted fingerprint must also account for the renderer version.
pub fn render_create_sql(model: &DatabaseModel) -> io::Result<String> {
	validate_model(model)?;

	let mut statements = Vec::new();
	for schema in &model.schemas {
		for table in &schema.tables {
			statements.push(render_table(table)?);
		}
	}
	for schema in &model.schemas {
		for table in &schema.tables {
			for index in &table.indexes {
				statements.push(render_index(&table.name, index)?);
			}
		}
	}

	if statements.is_empty() {
		Ok(String::new())
	} else {
		Ok(format!("{};", statements.join(";\n")))
	}
}

#[derive(Clone, Copy)]
enum ObjectKind {
	Table,
	Index,
}

impl ObjectKind {
	fn name(self) -> &'static str {
		match self {
			Self::Table => "table",
			Self::Index => "index",
		}
	}
}

fn validate_model(model: &DatabaseModel) -> io::Result<()> {
	let mut objects = BTreeMap::<String, (ObjectKind, &str)>::new();
	let mut tables = BTreeMap::<String, &str>::new();

	for schema in &model.schemas {
		reject_collection("views", schema.views.len())?;
		reject_collection("enums", schema.enums.len())?;
		reject_collection("domains", schema.domains.len())?;
		reject_collection("sequences", schema.sequences.len())?;

		for table in &schema.tables {
			register_object(&mut objects, ObjectKind::Table, &table.name)?;
			tables.insert(table.name.to_ascii_lowercase(), &table.name);
			validate_table(table)?;
		}
	}

	for schema in &model.schemas {
		for table in &schema.tables {
			for foreign_key in &table.foreign_keys {
				let folded = foreign_key.references_table.to_ascii_lowercase();
				if folded.starts_with("sqlite_") || folded.starts_with("__squealy_") {
					return Err(invalid(format!(
						"foreign key `{}` references reserved table `{}`",
						foreign_key.name, foreign_key.references_table
					)));
				}
				if !tables.contains_key(&folded) {
					return Err(invalid(format!(
						"foreign key `{}` references unknown table `{}`",
						foreign_key.name, foreign_key.references_table
					)));
				}
			}
			for index in &table.indexes {
				register_object(&mut objects, ObjectKind::Index, &index.name)?;
			}
		}
	}

	Ok(())
}

fn reject_collection(name: &str, count: usize) -> io::Result<()> {
	if count == 0 {
		Ok(())
	} else {
		Err(unsupported(format!(
			"SQLite create DDL does not support model {name}"
		)))
	}
}

fn register_object<'a>(
	objects: &mut BTreeMap<String, (ObjectKind, &'a str)>,
	kind: ObjectKind,
	name: &'a str,
) -> io::Result<()> {
	let folded = name.to_ascii_lowercase();
	if folded.starts_with("sqlite_") || folded.starts_with("__squealy_") {
		return Err(invalid(format!(
			"{} name `{name}` uses a reserved SQLite or Squealy prefix",
			kind.name()
		)));
	}
	if let Some((existing_kind, existing_name)) = objects.get(&folded) {
		return Err(invalid(format!(
			"{} name `{name}` collides case-insensitively with {} `{existing_name}` in SQLite's flattened namespace",
			kind.name(),
			existing_kind.name()
		)));
	}
	objects.insert(folded, (kind, name));
	Ok(())
}

fn validate_table(table: &TableModel) -> io::Result<()> {
	if table.comment.is_some() {
		return Err(unsupported(format!("table `{}` has a comment", table.name)));
	}
	reject_collection("table exclusions", table.exclusions.len())?;

	let mut columns = BTreeMap::<String, &str>::new();
	for column in &table.columns {
		let folded = column.name.to_ascii_lowercase();
		if let Some(existing) = columns.insert(folded, &column.name) {
			return Err(invalid(format!(
				"column `{}` on table `{}` collides case-insensitively with column `{existing}`",
				column.name, table.name
			)));
		}
		validate_column(&table.name, column)?;
	}

	if table.columns.is_empty() {
		return Err(invalid(format!(
			"table `{}` must contain at least one column",
			table.name
		)));
	}

	if let Some(primary_key) = &table.primary_key {
		validate_constraint(&table.name, "primary key", primary_key, &columns)?;
	}
	for unique in &table.uniques {
		validate_constraint(&table.name, "unique constraint", unique, &columns)?;
	}
	for check in &table.checks {
		validate_check(&table.name, check)?;
	}
	for foreign_key in &table.foreign_keys {
		validate_foreign_key(&table.name, foreign_key, &columns)?;
	}
	for index in &table.indexes {
		validate_index(&table.name, index, &columns)?;
	}

	Ok(())
}

fn validate_column(table: &str, column: &ColumnModel) -> io::Result<()> {
	if column.comment.is_some() {
		return Err(unsupported(format!(
			"column `{}` on table `{table}` has a comment",
			column.name
		)));
	}
	if column.collation.is_some() {
		return Err(unsupported(format!(
			"column `{}` on table `{table}` has a collation",
			column.name
		)));
	}
	if column.identity.is_some() {
		return Err(unsupported(format!(
			"column `{}` on table `{table}` is an identity column",
			column.name
		)));
	}
	if column.generated.is_some() {
		return Err(unsupported(format!(
			"column `{}` on table `{table}` is generated",
			column.name
		)));
	}
	if column.on_update.is_some() {
		return Err(unsupported(format!(
			"column `{}` on table `{table}` has ON UPDATE metadata",
			column.name
		)));
	}
	if !matches!(
		column.ty,
		SqlType::I64 | SqlType::Bool | SqlType::String | SqlType::Bytes | SqlType::FixedBytes(_)
	) {
		return Err(unsupported(format!(
			"column `{}` on table `{table}` uses unsupported SQLite DDL type {:?}",
			column.name, column.ty
		)));
	}
	Ok(())
}

fn validate_constraint(
	table: &str,
	kind: &str,
	constraint: &Constraint,
	columns: &BTreeMap<String, &str>,
) -> io::Result<()> {
	if !constraint.prefix_lengths.is_empty() {
		return Err(unsupported(format!(
			"{kind} `{}` on table `{table}` has column prefix lengths",
			constraint.name
		)));
	}
	validate_columns(table, kind, &constraint.columns, columns)
}

fn validate_check(table: &str, check: &CheckModel) -> io::Result<()> {
	if check.validation.is_some() {
		return Err(unsupported(format!(
			"check `{}` on table `{table}` has validation metadata",
			check.name
		)));
	}
	if check.enforcement.is_some() {
		return Err(unsupported(format!(
			"check `{}` on table `{table}` has enforcement metadata",
			check.name
		)));
	}
	Ok(())
}

fn validate_foreign_key(
	table: &str,
	foreign_key: &ForeignKeyModel,
	columns: &BTreeMap<String, &str>,
) -> io::Result<()> {
	validate_columns(table, "foreign key", &foreign_key.columns, columns)?;
	if foreign_key.references_columns.is_empty()
		|| foreign_key.references_columns.len() != foreign_key.columns.len()
	{
		return Err(invalid(format!(
			"foreign key `{}` on table `{table}` must reference the same non-zero number of columns",
			foreign_key.name
		)));
	}
	if foreign_key.validation.is_some() {
		return Err(unsupported(format!(
			"foreign key `{}` on table `{table}` has validation metadata",
			foreign_key.name
		)));
	}
	if foreign_key.enforcement.is_some() {
		return Err(unsupported(format!(
			"foreign key `{}` on table `{table}` has enforcement metadata",
			foreign_key.name
		)));
	}
	if matches!(foreign_key.match_type, Some(ForeignKeyMatch::Raw(_))) {
		return Err(unsupported(format!(
			"foreign key `{}` on table `{table}` has a raw match type",
			foreign_key.name
		)));
	}
	if matches!(
		foreign_key.deferrability,
		Some(ConstraintDeferrability::Raw(_))
	) {
		return Err(unsupported(format!(
			"foreign key `{}` on table `{table}` has raw deferrability",
			foreign_key.name
		)));
	}
	if matches!(foreign_key.on_delete, Some(ForeignKeyAction::Raw(_)))
		|| matches!(foreign_key.on_update, Some(ForeignKeyAction::Raw(_)))
	{
		return Err(unsupported(format!(
			"foreign key `{}` on table `{table}` has a raw referential action",
			foreign_key.name
		)));
	}
	Ok(())
}

fn validate_index(
	table: &str,
	index: &IndexModel,
	columns: &BTreeMap<String, &str>,
) -> io::Result<()> {
	if index.columns.is_empty() {
		return Err(invalid(format!(
			"index `{}` must contain at least one column",
			index.name
		)));
	}
	let unsupported_field = if !index.expressions.is_empty() {
		Some("expressions")
	} else if !index.include_columns.is_empty() {
		Some("included columns")
	} else if index.method.is_some() {
		Some("an index method")
	} else if !index.directions.is_empty() {
		Some("sort directions")
	} else if !index.nulls.is_empty() {
		Some("null ordering")
	} else if !index.collations.is_empty() {
		Some("collations")
	} else if !index.operator_classes.is_empty() {
		Some("operator classes")
	} else if !index.prefix_lengths.is_empty() {
		Some("column prefix lengths")
	} else if index.predicate.is_some() {
		Some("a predicate")
	} else {
		None
	};

	if let Some(field) = unsupported_field {
		return Err(unsupported(format!(
			"index `{}` has unsupported advanced metadata: {field}",
			index.name
		)));
	}
	validate_columns(table, "index", &index.columns, columns)
}

fn validate_columns(
	table: &str,
	kind: &str,
	names: &[String],
	columns: &BTreeMap<String, &str>,
) -> io::Result<()> {
	if names.is_empty() {
		return Err(invalid(format!(
			"{kind} on table `{table}` must contain at least one column"
		)));
	}
	for name in names {
		if !columns.contains_key(&name.to_ascii_lowercase()) {
			return Err(invalid(format!(
				"{kind} on table `{table}` references unknown column `{name}`"
			)));
		}
	}
	Ok(())
}

fn render_table(table: &TableModel) -> io::Result<String> {
	let mut writer = Vec::new();
	writer.write_all(b"CREATE TABLE ")?;
	write_quoted_ident(&table.name, &mut writer)?;
	writer.write_all(b" (\n")?;

	let mut clauses = Vec::new();
	for column in &table.columns {
		clauses.push(render_column(column)?);
	}
	if let Some(primary_key) = &table.primary_key {
		clauses.push(render_key_constraint("PRIMARY KEY", primary_key)?);
	}
	for unique in &table.uniques {
		clauses.push(render_key_constraint("UNIQUE", unique)?);
	}
	for check in &table.checks {
		clauses.push(render_check(check)?);
	}
	for foreign_key in &table.foreign_keys {
		clauses.push(render_foreign_key(foreign_key)?);
	}

	for (index, clause) in clauses.iter().enumerate() {
		if index > 0 {
			writer.write_all(b",\n")?;
		}
		writer.write_all(b"    ")?;
		writer.write_all(clause)?;
	}
	writer.write_all(b"\n)")?;
	String::from_utf8(writer).map_err(|error| invalid(error.to_string()))
}

fn render_column(column: &ColumnModel) -> io::Result<Vec<u8>> {
	let mut writer = Vec::new();
	write_quoted_ident(&column.name, &mut writer)?;
	writer.write_all(b" ")?;
	writer.write_all(column_type(&column.ty).as_bytes())?;
	if !column.nullable {
		writer.write_all(b" NOT NULL")?;
	}
	if let Some(default) = &column.default {
		writer.write_all(b" DEFAULT ")?;
		render_default(default, &mut writer)?;
	}
	if let SqlType::FixedBytes(width) = column.ty {
		writer.write_all(b" CHECK (length(CAST(")?;
		write_quoted_ident(&column.name, &mut writer)?;
		write!(writer, " AS BLOB)) = {width})")?;
	}
	Ok(writer)
}

fn render_key_constraint(keyword: &str, constraint: &Constraint) -> io::Result<Vec<u8>> {
	let mut writer = Vec::new();
	writer.write_all(keyword.as_bytes())?;
	writer.write_all(b" (")?;
	write_ident_list(&constraint.columns, &mut writer)?;
	writer.write_all(b")")?;
	Ok(writer)
}

fn render_check(check: &CheckModel) -> io::Result<Vec<u8>> {
	let mut writer = Vec::new();
	writer.write_all(b"CHECK (")?;
	render_scalar_expr(&check.expression, &SqliteDialect, &mut writer)?;
	writer.write_all(b")")?;
	Ok(writer)
}

fn render_foreign_key(foreign_key: &ForeignKeyModel) -> io::Result<Vec<u8>> {
	let mut writer = Vec::new();
	writer.write_all(b"FOREIGN KEY (")?;
	write_ident_list(&foreign_key.columns, &mut writer)?;
	writer.write_all(b") REFERENCES ")?;
	write_quoted_ident(&foreign_key.references_table, &mut writer)?;
	writer.write_all(b" (")?;
	write_ident_list(&foreign_key.references_columns, &mut writer)?;
	writer.write_all(b")")?;
	if let Some(match_type) = &foreign_key.match_type {
		write!(writer, " MATCH {}", match_type.as_sql())?;
	}
	if let Some(action) = &foreign_key.on_delete {
		write!(writer, " ON DELETE {}", action.as_sql())?;
	}
	if let Some(action) = &foreign_key.on_update {
		write!(writer, " ON UPDATE {}", action.as_sql())?;
	}
	if let Some(deferrability) = &foreign_key.deferrability {
		write!(writer, " {}", deferrability.as_sql())?;
	}
	Ok(writer)
}

fn render_index(table: &str, index: &IndexModel) -> io::Result<String> {
	let mut writer = Vec::new();
	writer.write_all(if index.unique {
		b"CREATE UNIQUE INDEX ".as_slice()
	} else {
		b"CREATE INDEX ".as_slice()
	})?;
	write_quoted_ident(&index.name, &mut writer)?;
	writer.write_all(b" ON ")?;
	write_quoted_ident(table, &mut writer)?;
	writer.write_all(b" (")?;
	write_ident_list(&index.columns, &mut writer)?;
	writer.write_all(b")")?;
	String::from_utf8(writer).map_err(|error| invalid(error.to_string()))
}

fn column_type(ty: &SqlType) -> &'static str {
	match ty {
		SqlType::I64 | SqlType::Bool => "INTEGER",
		SqlType::String => "TEXT",
		SqlType::Bytes | SqlType::FixedBytes(_) => "BLOB",
		_ => unreachable!("column types are validated before rendering"),
	}
}

fn render_default(default: &DefaultValue, writer: &mut dyn Write) -> io::Result<()> {
	match default {
		DefaultValue::Null => writer.write_all(b"NULL"),
		DefaultValue::Int(value) => write!(writer, "{value}"),
		DefaultValue::UInt(value) => write!(writer, "{value}"),
		DefaultValue::Float(value) if value.is_finite() => write!(writer, "{value}"),
		DefaultValue::Float(_) => Err(unsupported(
			"SQLite DDL cannot render a non-finite floating-point default",
		)),
		DefaultValue::Text(value) => write_text_literal(value, writer),
		DefaultValue::Bool(value) => writer.write_all(if *value { b"1" } else { b"0" }),
		DefaultValue::CurrentTimestamp => writer.write_all(b"CURRENT_TIMESTAMP"),
		DefaultValue::CurrentDate => writer.write_all(b"CURRENT_DATE"),
		DefaultValue::CurrentTime => writer.write_all(b"CURRENT_TIME"),
		DefaultValue::Raw(value) => writer.write_all(value.as_bytes()),
	}
}

fn write_ident_list(names: &[String], writer: &mut dyn Write) -> io::Result<()> {
	for (index, name) in names.iter().enumerate() {
		if index > 0 {
			writer.write_all(b", ")?;
		}
		write_quoted_ident(name, writer)?;
	}
	Ok(())
}

fn write_quoted_ident(value: &str, writer: &mut dyn Write) -> io::Result<()> {
	write_delimited(value, '"', writer)
}

fn write_text_literal(value: &str, writer: &mut dyn Write) -> io::Result<()> {
	if value.contains('\0') {
		return Err(invalid("SQLite text literals cannot contain a NUL byte"));
	}
	write_delimited(value, '\'', writer)
}

fn write_delimited(value: &str, delimiter: char, writer: &mut dyn Write) -> io::Result<()> {
	write!(writer, "{delimiter}")?;
	let mut start = 0;
	for (index, _) in value.match_indices(delimiter) {
		writer.write_all(&value.as_bytes()[start..index])?;
		write!(writer, "{delimiter}{delimiter}")?;
		start = index + delimiter.len_utf8();
	}
	writer.write_all(&value.as_bytes()[start..])?;
	write!(writer, "{delimiter}")
}

fn invalid(message: impl Into<String>) -> io::Error {
	io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn unsupported(message: impl Into<String>) -> io::Error {
	io::Error::new(io::ErrorKind::Unsupported, message.into())
}

#[derive(Clone, Copy)]
struct SqliteDialect;

impl Dialect for SqliteDialect {
	fn write_placeholder(&self, _index: usize, writer: &mut dyn Write) -> io::Result<()> {
		writer.write_all(b"?")
	}

	fn write_quoted_ident(&self, ident: &str, writer: &mut dyn Write) -> io::Result<()> {
		write_quoted_ident(ident, writer)
	}

	fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
		if let SqlType::Enum(name) = ty {
			return Err(unsupported(format!(
				"SQLite cannot render a CAST to enum `{name}`"
			)));
		}
		writer.write_all(sqlite_affinity(ty).as_bytes())
	}

	fn write_general_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
		squealy_core::reject_128bit_general_cast(ty)?;
		if matches!(ty, SqlType::Decimal { .. }) {
			return Err(unsupported(
				"SQLite cannot faithfully render a general DECIMAL/NUMERIC cast",
			));
		}
		self.write_cast_type(ty, writer)
	}

	fn unary_string_fn_name(&self, function: squealy_core::UnaryStringFunc) -> &'static str {
		match function {
			squealy_core::UnaryStringFunc::Length => "length",
			other => other.sql_name(),
		}
	}

	fn qualify_schema(&self) -> bool {
		false
	}

	fn substring_uses_function_call(&self) -> bool {
		true
	}

	fn concat_uses_pipe_operator(&self) -> bool {
		true
	}
}

fn sqlite_affinity(ty: &SqlType) -> &str {
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
		| SqlType::Jsonb
		| SqlType::Enum(_) => "TEXT",
		SqlType::Bytes | SqlType::FixedBytes(_) => "BLOB",
		SqlType::Raw(raw) => raw,
	}
}
