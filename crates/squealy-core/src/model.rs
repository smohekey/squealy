//! Owned, backend-neutral schema model.
//!
//! The compile-time `#[derive(Table/Schema/Database)]` types are the source of truth.
//! [`DatabaseModel::from_database`] materializes them into an owned model that DDL-management
//! operations (render create-from-scratch, package export/import, future diff) all consume. The same
//! model can later be produced from a package or from live-database introspection, so operations stay
//! source-agnostic.
//!
//! These types live in the core crate (rather than the `squealy-model` engine) so that backends can
//! implement [`SchemaBackend`](crate::SchemaBackend) against them without depending on the engine.
//! See `docs/ddl-management.md` for the design.

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
		// Sequences and domains are not authored through the derive macros (they are introspected /
		// KDL-authored), so a compile-time schema declares none.
		sequences: Vec::new(),
		domains: Vec::new(),
	}
}

fn view_from_dyn(view: &(dyn crate::ViewDef + Sync)) -> ViewModel {
	let columns = view.columns();
	let mut query = view.definition_model();
	// The view's declared column list authoritatively names the outputs: the renderer emits it (`CREATE
	// VIEW v (id, name) AS …`) and suppresses each projection's own `AS` alias, so the builder-internal
	// projection names (`t0_id`) never reach the SQL. Set the top-level projection output names to the
	// declared column names positionally, so the authored body matches the form introspection
	// reconstructs — a deparse (`pg_get_viewdef`) carries no column list, so its projection is named by
	// the view's output columns — and a published view re-plans to empty instead of churning. Lengths
	// match by construction (the `Row` check ties each projected column to one declared output).
	for (item, column) in query.projection.iter_mut().zip(&columns) {
		item.output_name = column.name.clone();
	}
	ViewModel {
		name: view.name().to_owned(),
		comment: None,
		columns,
		// The typed view builder only produces a single `SELECT`; set-op/CTE view bodies are
		// reconstructed on introspection, not authored through the builder (Track F, a follow-up).
		query: crate::ViewBody::Select(Box::new(query)),
		// The typed view builder produces only regular views; a materialized view is modeled through
		// introspection / a package, not the derive builder.
		materialized: false,
	}
}

/// Builds the neutral [`TableModel`] for a query-builder [`Table`]. This is the canonical
/// `&dyn Table` → model conversion used when lowering a whole database; a backend can reuse it so its
/// single-table create path (`Backend::write_table`) renders identically to its model-based
/// create-from-scratch path.
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
		// The derive macro has no `ON UPDATE` attribute; the value arrives only from introspection or a
		// KDL package.
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

// Deterministic constraint/index names. These double as the identity the future diff uses to match
// constraints across versions, so the conventions are stable and documented.

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
