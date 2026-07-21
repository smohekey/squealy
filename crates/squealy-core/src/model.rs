//! Owned, backend-neutral schema model.
//!
//! The compile-time `#[derive(Table/Schema/Database)]` types are the source of truth.
//! [`DatabaseModel::from_database`] materializes their tables, constraints, indexes, and views into
//! an owned model. The complete public model also represents metadata kinds that derives do not
//! currently author, including enums, sequences, domains, exclusions, and materialized views.

use crate::{
	CheckModel, ColumnModel, Constraint, DefaultValue, ForeignKeyAction, ForeignKeyModel,
	GeneratedColumnModel, GeneratedStorage, IdentityMode, IdentityModel, IndexModel, SchemaModel,
	SqlType, TableModel, ViewColumnModel, ViewModel, ViewQueryModel,
};
use crate::{
	Column, ColumnDefault, ColumnType, Database, DatabaseSchema, ForeignKey, Index, Table,
};

/// An owned, backend-neutral model of a whole database.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DatabaseModel {
	pub schemas: Vec<SchemaModel>,
}

impl From<ColumnType> for SqlType {
	fn from(column_type: ColumnType) -> Self {
		match column_type {
			ColumnType::I8 => SqlType::I8,
			ColumnType::I16 => SqlType::I16,
			ColumnType::I32 => SqlType::I32,
			ColumnType::I64 => SqlType::I64,
			ColumnType::I128 => SqlType::I128,
			ColumnType::Isize => SqlType::Isize,
			ColumnType::U8 => SqlType::U8,
			ColumnType::U16 => SqlType::U16,
			ColumnType::U32 => SqlType::U32,
			ColumnType::U64 => SqlType::U64,
			ColumnType::U128 => SqlType::U128,
			ColumnType::Usize => SqlType::Usize,
			ColumnType::F32 => SqlType::F32,
			ColumnType::F64 => SqlType::F64,
			ColumnType::String => SqlType::String,
			ColumnType::Bool => SqlType::Bool,
			ColumnType::Varchar(length) => SqlType::Varchar(length),
			ColumnType::Char(length) => SqlType::Char(length),
			ColumnType::Text => SqlType::Text,
			ColumnType::Decimal { precision, scale } => SqlType::Decimal { precision, scale },
			ColumnType::Date => SqlType::Date,
			ColumnType::Time { tz, precision } => SqlType::Time { tz, precision },
			ColumnType::Timestamp { tz, precision } => SqlType::Timestamp { tz, precision },
			ColumnType::Uuid => SqlType::Uuid,
			ColumnType::Json => SqlType::Json,
			ColumnType::Jsonb => SqlType::Jsonb,
			ColumnType::Bytes => SqlType::Bytes,
			ColumnType::FixedBytes(width) => SqlType::FixedBytes(width),
			ColumnType::Raw(raw) => SqlType::Raw(raw.to_owned()),
		}
	}
}

impl From<ColumnDefault> for DefaultValue {
	fn from(default: ColumnDefault) -> Self {
		match default {
			ColumnDefault::Null => DefaultValue::Null,
			ColumnDefault::Int(value) => DefaultValue::Int(value),
			ColumnDefault::UInt(value) => DefaultValue::UInt(value),
			ColumnDefault::Float(value) => DefaultValue::Float(value),
			ColumnDefault::Text(value) => DefaultValue::Text(value.to_owned()),
			ColumnDefault::Bool(value) => DefaultValue::Bool(value),
			ColumnDefault::CurrentTimestamp => DefaultValue::CurrentTimestamp,
			ColumnDefault::CurrentDate => DefaultValue::CurrentDate,
			ColumnDefault::CurrentTime => DefaultValue::CurrentTime,
			ColumnDefault::Raw(value) => DefaultValue::Raw(value.to_owned()),
		}
	}
}

/// Object-safe runtime metadata and body lowering for a view, consumed by the model walker.
///
/// The `#[derive(View)]` macro generates an implementation: the typed `ViewDefinition` the user writes
/// is lowered into [`Self::definition_model`] through the canonical model sink.
pub trait ViewDef: Sync {
	fn schema_name(&self) -> Option<&'static str>;

	fn name(&self) -> &'static str;

	/// The view's output columns, in projection order.
	fn columns(&self) -> Vec<ViewColumnModel>;

	/// The structural body of the view's `SELECT`, with literals inlined.
	fn definition_model(&self) -> ViewQueryModel;
}

impl DatabaseModel {
	/// Walks a compile-time [`Database`] into an owned model.
	pub fn from_database<D: Database>() -> Self {
		Self {
			schemas: D::schemas().map(schema_from_dyn).collect(),
		}
	}
}

fn schema_from_dyn(schema: &(dyn DatabaseSchema + Sync)) -> SchemaModel {
	SchemaModel {
		name: schema.name().map(str::to_owned),
		tables: schema.tables().map(table_from_dyn).collect(),
		views: schema.views().map(view_from_dyn).collect(),
		enums: Vec::new(),
		// Sequences and domains are represented by the owned model but are not authored by derives.
		sequences: Vec::new(),
		domains: Vec::new(),
	}
}

fn view_from_dyn(view: &(dyn crate::ViewDef + Sync)) -> ViewModel {
	let columns = view.columns();
	let mut query = view.definition_model();
	// The declared column list authoritatively names the outputs. Replace builder-internal projection
	// names (`t0_id`) positionally so rendering the typed view body uses its public column names.
	// Lengths match by construction: the `Row` check ties each projection to one declared output.
	for (item, column) in query.projection.iter_mut().zip(&columns) {
		item.output_name = column.name.clone();
	}
	ViewModel {
		name: view.name().to_owned(),
		comment: None,
		columns,
		// The typed view builder currently authors a single `SELECT` body.
		query: crate::ViewBody::Select(Box::new(query)),
		// The derive authors regular views; the owned type still represents materialized views.
		materialized: false,
	}
}

/// Builds the neutral [`TableModel`] for a query-builder [`Table`].
///
/// This is the canonical `&dyn Table` to owned-metadata conversion used when lowering a database.
pub fn table_from_dyn(table: &(dyn Table + Sync)) -> TableModel {
	let name = table.name().to_owned();
	let columns = table.columns();

	// Prefer an explicit table-level primary key (which carries column ordering and an optional
	// name); otherwise hoist every column marked `#[column(primary_key)]` into one constraint.
	let primary_key = match table.primary_key() {
		Some(pk) => Some(Constraint {
			prefix_lengths: Vec::new(),
			name: pk.name.map(str::to_owned).unwrap_or_else(|| pk_name(&name)),
			columns: pk
				.columns
				.iter()
				.map(|column| (*column).to_owned())
				.collect(),
		}),
		None => {
			let pk_columns = columns
				.iter()
				.filter(|column| column.primary_key())
				.map(|column| column.name().to_owned())
				.collect::<Vec<_>>();
			(!pk_columns.is_empty()).then(|| Constraint {
				prefix_lengths: Vec::new(),
				name: pk_name(&name),
				columns: pk_columns,
			})
		}
	};

	// Single-column `#[column(unique)]` markers, then table-level `#[unique(columns = [..])]`
	// composite constraints. The latter carry an optional explicit name and otherwise fall back to
	// the same deterministic `uq_<table>_<columns>` convention. A unique that carries a
	// `where = ...` predicate is excluded here: Postgres cannot attach a `WHERE` to a table
	// constraint, so it is lowered to a partial unique index below (sharing the `uq_` name).
	let uniques = columns
		.iter()
		.filter(|column| column.unique() && column.unique_predicate().is_none())
		.map(|column| Constraint {
			prefix_lengths: Vec::new(),
			name: uq_name(&name, &[column.name()]),
			columns: vec![column.name().to_owned()],
		})
		.chain(
			table
				.uniques()
				.iter()
				.filter(|unique| unique.predicate.is_none())
				.map(|unique| Constraint {
					prefix_lengths: Vec::new(),
					name: unique
						.name
						.map(str::to_owned)
						.unwrap_or_else(|| uq_name(&name, unique.columns)),
					columns: unique
						.columns
						.iter()
						.map(|column| (*column).to_owned())
						.collect(),
				}),
		)
		.collect();

	let foreign_keys = columns
		.iter()
		.filter_map(|column| {
			column
				.references()
				.map(|reference| foreign_key_from_dyn(&name, column.name(), reference))
		})
		.collect();

	let checks = columns
		.iter()
		.filter_map(|column| {
			column.check().map(|expression| CheckModel {
				name: ck_name(&name, column.name()),
				expression,
				validation: None,
				enforcement: None,
			})
		})
		.collect();

	// Predicated uniques (single-column `#[column(unique, where = ...)]` and table-level
	// `#[unique(columns = [..], where = ...)]`) become partial unique indexes, appended after the
	// table's own `#[index(..)]` declarations.
	let partial_unique_indexes = columns
		.iter()
		.filter_map(|column| {
			column.unique_predicate().map(|predicate| {
				partial_unique_index(
					uq_name(&name, &[column.name()]),
					vec![column.name().to_owned()],
					predicate,
				)
			})
		})
		.chain(table.uniques().iter().filter_map(|unique| {
			unique.predicate.map(|predicate| {
				partial_unique_index(
					unique
						.name
						.map(str::to_owned)
						.unwrap_or_else(|| uq_name(&name, unique.columns)),
					unique
						.columns
						.iter()
						.map(|column| (*column).to_owned())
						.collect(),
					predicate,
				)
			})
		}));

	let indexes = table
		.indexes()
		.iter()
		.map(|index| index_from_dyn(&name, *index))
		.chain(partial_unique_indexes)
		.collect();

	TableModel {
		name,
		comment: None,
		columns: columns
			.iter()
			.map(|column| column_from_dyn(*column))
			.collect(),
		primary_key,
		foreign_keys,
		uniques,
		checks,
		indexes,
		exclusions: Vec::new(),
	}
}

fn column_from_dyn(column: &dyn Column) -> ColumnModel {
	ColumnModel {
		name: column.name().to_owned(),
		comment: None,
		ty: column.column_type().into(),
		collation: None,
		nullable: column.nullable(),
		default: column.default().map(DefaultValue::from),
		identity: column.auto_increment().then_some(IdentityModel {
			mode: IdentityMode::ByDefault,
		}),
		generated: column.generated().then_some(GeneratedColumnModel {
			// The `#[column(generated)]` attribute marks the column generated but supplies no
			// expression, so a macro-built model has none; the renderer rejects such a column.
			expression: None,
			storage: GeneratedStorage::Unknown,
		}),
		// The derive macro has no `ON UPDATE` attribute.
		on_update: None,
	}
}

fn foreign_key_from_dyn(table: &str, column: &str, reference: &dyn ForeignKey) -> ForeignKeyModel {
	ForeignKeyModel {
		name: fk_name(table, &[column]),
		columns: vec![column.to_owned()],
		references_schema: reference.schema_name().map(str::to_owned),
		references_table: reference.table().to_owned(),
		references_columns: vec![reference.column().to_owned()],
		match_type: None,
		deferrability: None,
		validation: None,
		enforcement: None,
		on_delete: reference.on_delete().map(ForeignKeyAction::from_sql),
		on_update: reference.on_update().map(ForeignKeyAction::from_sql),
	}
}

fn index_from_dyn(table: &str, index: &dyn Index) -> IndexModel {
	let columns = index.columns();
	IndexModel {
		name: index
			.name()
			.map(str::to_owned)
			.unwrap_or_else(|| idx_name(table, columns)),
		columns: columns.iter().map(|column| (*column).to_owned()).collect(),
		expressions: Vec::new(),
		include_columns: Vec::new(),
		unique: index.unique(),
		method: None,
		directions: Vec::new(),
		nulls: Vec::new(),
		collations: Vec::new(),
		operator_classes: Vec::new(),
		prefix_lengths: Vec::new(),
		predicate: index.predicate().map(|predicate| Box::new(predicate())),
	}
}

/// A partial unique index synthesized from a predicated `#[column(unique, where = ...)]` or
/// `#[unique(columns = [..], where = ...)]` declaration. It keeps the `uq_<table>_<columns>`
/// identity of the constraint it replaces, but renders as `CREATE UNIQUE INDEX ... WHERE ...`.
fn partial_unique_index(
	name: String,
	columns: Vec<String>,
	predicate: fn() -> crate::ExprNode,
) -> IndexModel {
	IndexModel {
		name,
		columns,
		expressions: Vec::new(),
		include_columns: Vec::new(),
		unique: true,
		method: None,
		directions: Vec::new(),
		nulls: Vec::new(),
		collations: Vec::new(),
		operator_classes: Vec::new(),
		prefix_lengths: Vec::new(),
		predicate: Some(Box::new(predicate())),
	}
}

// Deterministic constraint/index names keep exported metadata stable across builds.

fn join_columns(columns: &[&str]) -> String {
	columns.join("_")
}

fn pk_name(table: &str) -> String {
	format!("pk_{table}")
}

fn uq_name(table: &str, columns: &[&str]) -> String {
	format!("uq_{table}_{}", join_columns(columns))
}

fn fk_name(table: &str, columns: &[&str]) -> String {
	format!("fk_{table}_{}", join_columns(columns))
}

fn ck_name(table: &str, column: &str) -> String {
	format!("ck_{table}_{column}")
}

fn idx_name(table: &str, columns: &[&str]) -> String {
	format!("idx_{table}_{}", join_columns(columns))
}
