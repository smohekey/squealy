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
    Column, ColumnDefault, ColumnType, Database, DatabaseSchema, ForeignKey, Index, Table,
};

/// An owned, backend-neutral model of a whole database.
#[derive(Clone, Debug, PartialEq)]
pub struct DatabaseModel {
    pub schemas: Vec<SchemaModel>,
}

/// A namespace within a database (a SQL "schema").
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaModel {
    /// The namespace name, or `None` for the default/unqualified namespace.
    pub name: Option<String>,
    pub tables: Vec<TableModel>,
}

/// A table and its table-level, named constraints.
///
/// Unlike the query-side [`Column`] trait (which hangs primary-key/unique/foreign-key/check facts off
/// each column), the model hoists those into named table-level lists. This matches `ALTER TABLE … ADD
/// CONSTRAINT`, how catalogs report constraints during introspection, and admits composite keys.
#[derive(Clone, Debug, PartialEq)]
pub struct TableModel {
    pub name: String,
    pub columns: Vec<ColumnModel>,
    pub primary_key: Option<Constraint>,
    pub foreign_keys: Vec<ForeignKeyModel>,
    pub uniques: Vec<Constraint>,
    pub checks: Vec<CheckModel>,
    pub indexes: Vec<IndexModel>,
}

/// Per-column facts (the table-level constraints live on [`TableModel`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnModel {
    pub name: String,
    pub ty: SqlType,
    pub nullable: bool,
    pub default: Option<DefaultValue>,
    pub auto_increment: bool,
    pub generated: bool,
}

/// The owned, backend-neutral logical column type.
///
/// This mirrors the compile-time [`ColumnType`] but owns its strings, so a model can be rebuilt from
/// a package or live-database introspection (the `const`-friendly [`ColumnType`] borrows `'static`
/// strings and cannot represent runtime-sourced values). It is the place the neutral type vocabulary
/// grows structurally (e.g. `Varchar { len }`) as introspection lands.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlType {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    F32,
    F64,
    String,
    Bool,
    Varchar(u32),
    Char(u32),
    Text,
    Decimal {
        precision: u32,
        scale: u32,
    },
    Date,
    Time {
        tz: bool,
    },
    Timestamp {
        tz: bool,
    },
    Uuid,
    Json,
    Jsonb,
    Bytes,
    /// A backend-specific type name, emitted verbatim into DDL.
    Raw(String),
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
            ColumnType::Time { tz } => SqlType::Time { tz },
            ColumnType::Timestamp { tz } => SqlType::Timestamp { tz },
            ColumnType::Uuid => SqlType::Uuid,
            ColumnType::Json => SqlType::Json,
            ColumnType::Jsonb => SqlType::Jsonb,
            ColumnType::Bytes => SqlType::Bytes,
            ColumnType::Raw(raw) => SqlType::Raw(raw.to_owned()),
        }
    }
}

/// The owned, backend-neutral mirror of [`ColumnDefault`] (owns its strings; see [`SqlType`]).
#[derive(Clone, Debug, PartialEq)]
pub enum DefaultValue {
    Null,
    Int(i128),
    UInt(u128),
    Float(f64),
    Text(String),
    Bool(bool),
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
    /// A backend-specific default expression, emitted verbatim into DDL.
    Raw(String),
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

/// A named constraint over one or more columns (primary key, unique).
#[derive(Clone, Debug, PartialEq)]
pub struct Constraint {
    pub name: String,
    pub columns: Vec<String>,
}

/// A named foreign-key constraint.
#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKeyModel {
    pub name: String,
    pub columns: Vec<String>,
    pub references_schema: Option<String>,
    pub references_table: String,
    pub references_columns: Vec<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
}

/// A named check constraint carrying a backend-specific boolean expression.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckModel {
    pub name: String,
    pub expression: String,
}

/// A named index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexModel {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
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
    }
}

fn table_from_dyn(table: &(dyn Table + Sync)) -> TableModel {
    let name = table.name().to_owned();
    let columns = table.columns();

    let pk_columns = columns
        .iter()
        .filter(|column| column.primary_key())
        .map(|column| column.name().to_owned())
        .collect::<Vec<_>>();
    let primary_key = (!pk_columns.is_empty()).then(|| Constraint {
        name: pk_name(&name),
        columns: pk_columns,
    });

    let uniques = columns
        .iter()
        .filter(|column| column.unique())
        .map(|column| Constraint {
            name: uq_name(&name, &[column.name()]),
            columns: vec![column.name().to_owned()],
        })
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
                expression: expression.to_owned(),
            })
        })
        .collect();

    let indexes = table
        .indexes()
        .iter()
        .map(|index| index_from_dyn(&name, *index))
        .collect();

    TableModel {
        name,
        columns: columns
            .iter()
            .map(|column| column_from_dyn(*column))
            .collect(),
        primary_key,
        foreign_keys,
        uniques,
        checks,
        indexes,
    }
}

fn column_from_dyn(column: &dyn Column) -> ColumnModel {
    ColumnModel {
        name: column.name().to_owned(),
        ty: column.column_type().into(),
        nullable: column.nullable(),
        default: column.default().map(DefaultValue::from),
        auto_increment: column.auto_increment(),
        generated: column.generated(),
    }
}

fn foreign_key_from_dyn(table: &str, column: &str, reference: &dyn ForeignKey) -> ForeignKeyModel {
    ForeignKeyModel {
        name: fk_name(table, &[column]),
        columns: vec![column.to_owned()],
        references_schema: reference.schema_name().map(str::to_owned),
        references_table: reference.table().to_owned(),
        references_columns: vec![reference.column().to_owned()],
        on_delete: reference.on_delete().map(str::to_owned),
        on_update: reference.on_update().map(str::to_owned),
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
        unique: index.unique(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use squealy::*;

    #[derive(Clone, Debug, PartialEq, Table)]
    #[schema(Public)]
    struct User<'scope, C: ColumnMode = ColumnExpr> {
        #[column(primary_key, auto_increment)]
        id: C::Type<'scope, i32>,
        #[column(unique)]
        email: C::Type<'scope, String>,
        #[column(index)]
        name: C::Type<'scope, String>,
        #[column(db_type = "varchar(16)")]
        code: C::Type<'scope, String>,
        #[column(db_type = "jsonb")]
        payload: C::Type<'scope, String>,
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    #[schema(Public)]
    struct Post<'scope, C: ColumnMode = ColumnExpr> {
        #[column(primary_key, auto_increment)]
        id: C::Type<'scope, i32>,
        #[column(references(User::id, on_delete = "cascade"))]
        user_id: C::Type<'scope, i32>,
        title: C::Type<'scope, String>,
    }

    #[allow(dead_code)]
    #[derive(Schema)]
    struct Public {
        users: User<'static, ColumnName>,
        posts: Post<'static, ColumnName>,
    }

    #[allow(dead_code)]
    #[derive(Database)]
    struct App {
        public: Public,
    }

    fn table<'a>(model: &'a DatabaseModel, name: &str) -> &'a TableModel {
        model.schemas[0]
            .tables
            .iter()
            .find(|table| table.name == name)
            .unwrap_or_else(|| panic!("table `{name}` not found"))
    }

    #[test]
    fn walks_schema_and_tables() {
        let model = DatabaseModel::from_database::<App>();

        assert_eq!(model.schemas.len(), 1);
        assert_eq!(model.schemas[0].name.as_deref(), Some("public"));
        assert_eq!(model.schemas[0].tables.len(), 2);
    }

    #[test]
    fn hoists_primary_key_unique_and_index() {
        let model = DatabaseModel::from_database::<App>();
        let users = table(&model, "users");

        assert_eq!(
            users.primary_key,
            Some(Constraint {
                name: "pk_users".to_owned(),
                columns: vec!["id".to_owned()],
            })
        );
        assert_eq!(
            users.uniques,
            vec![Constraint {
                name: "uq_users_email".to_owned(),
                columns: vec!["email".to_owned()],
            }]
        );
        // `#[column(index)]` on `name` surfaces as a table-level index.
        assert!(
            users
                .indexes
                .iter()
                .any(|index| index.columns == vec!["name".to_owned()]),
            "expected an index on `name`: {:?}",
            users.indexes
        );

        let id = &users.columns[0];
        assert_eq!(id.name, "id");
        assert_eq!(id.ty, SqlType::I32);
        assert!(id.auto_increment);
        assert!(!id.nullable);
    }

    #[test]
    fn maps_db_type_columns_into_structured_sql_types() {
        // `#[column(db_type = "...")]` parses to a structured `ColumnType` in the derive, which the
        // walker carries through `From<ColumnType> for SqlType` into the owned model.
        let model = DatabaseModel::from_database::<App>();
        let users = table(&model, "users");

        let column = |name: &str| {
            users
                .columns
                .iter()
                .find(|column| column.name == name)
                .unwrap_or_else(|| panic!("column `{name}` not found"))
        };

        assert_eq!(column("code").ty, SqlType::Varchar(16));
        assert_eq!(column("payload").ty, SqlType::Jsonb);
    }

    #[test]
    fn hoists_foreign_key() {
        let model = DatabaseModel::from_database::<App>();
        let posts = table(&model, "posts");

        assert_eq!(
            posts.foreign_keys,
            vec![ForeignKeyModel {
                name: "fk_posts_user_id".to_owned(),
                columns: vec!["user_id".to_owned()],
                references_schema: Some("public".to_owned()),
                references_table: "users".to_owned(),
                references_columns: vec!["id".to_owned()],
                on_delete: Some("cascade".to_owned()),
                on_update: None,
            }]
        );
    }
}
