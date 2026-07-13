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
use squealy_ir::{
    CheckModel, ColumnModel, Constraint, DefaultValue, ForeignKeyAction, ForeignKeyModel,
    GeneratedColumnModel, GeneratedStorage, IdentityMode, IdentityModel, IndexModel, SchemaModel,
    SqlType, TableModel, ViewColumnModel, ViewModel, ViewQueryModel,
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
            name: uq_name(&name, &[column.name()]),
            columns: vec![column.name().to_owned()],
        })
        .chain(
            table
                .uniques()
                .iter()
                .filter(|unique| unique.predicate.is_none())
                .map(|unique| Constraint {
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
        assert_eq!(
            id.identity,
            Some(IdentityModel {
                mode: IdentityMode::ByDefault
            })
        );
        assert!(!id.nullable);
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    #[schema(Public)]
    #[primary_key(columns = [tenant_id, id])]
    struct Membership<'scope, C: ColumnMode = ColumnExpr> {
        tenant_id: C::Type<'scope, i64>,
        id: C::Type<'scope, i64>,
        role: C::Type<'scope, String>,
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    #[schema(Public)]
    #[primary_key(name = "membership_pk", columns = [tenant_id, id])]
    struct NamedMembership<'scope, C: ColumnMode = ColumnExpr> {
        tenant_id: C::Type<'scope, i64>,
        id: C::Type<'scope, i64>,
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    #[schema(Public)]
    #[unique(columns = [organization_id, slug])]
    struct Repository<'scope, C: ColumnMode = ColumnExpr> {
        #[column(primary_key)]
        id: C::Type<'scope, i64>,
        organization_id: C::Type<'scope, i64>,
        slug: C::Type<'scope, String>,
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    #[schema(Public)]
    #[unique(name = "uq_widget_sku", columns = [tenant_id, sku])]
    #[unique(columns = [tenant_id, label])]
    struct Widget<'scope, C: ColumnMode = ColumnExpr> {
        #[column(primary_key)]
        id: C::Type<'scope, i64>,
        #[column(unique)]
        tenant_id: C::Type<'scope, i64>,
        sku: C::Type<'scope, String>,
        label: C::Type<'scope, String>,
    }

    #[allow(dead_code)]
    #[derive(Schema)]
    struct CompositePublic {
        memberships: Membership<'static, ColumnName>,
        named_memberships: NamedMembership<'static, ColumnName>,
        repositorys: Repository<'static, ColumnName>,
        widgets: Widget<'static, ColumnName>,
    }

    #[allow(dead_code)]
    #[derive(Database)]
    struct CompositeApp {
        public: CompositePublic,
    }

    #[test]
    fn table_level_compound_primary_key_preserves_column_order() {
        let model = DatabaseModel::from_database::<CompositeApp>();
        let memberships = table(&model, "memberships");

        assert_eq!(
            memberships.primary_key,
            Some(Constraint {
                name: "pk_memberships".to_owned(),
                columns: vec!["tenant_id".to_owned(), "id".to_owned()],
            })
        );
    }

    #[test]
    fn table_level_primary_key_name_override_flows_through() {
        let model = DatabaseModel::from_database::<CompositeApp>();
        let named = table(&model, "named_memberships");

        assert_eq!(
            named.primary_key,
            Some(Constraint {
                name: "membership_pk".to_owned(),
                columns: vec!["tenant_id".to_owned(), "id".to_owned()],
            })
        );
    }

    #[test]
    fn table_level_composite_unique_emits_constraint() {
        let model = DatabaseModel::from_database::<CompositeApp>();
        let repository = table(&model, "repositorys");

        assert_eq!(
            repository.uniques,
            vec![Constraint {
                name: "uq_repositorys_organization_id_slug".to_owned(),
                columns: vec!["organization_id".to_owned(), "slug".to_owned()],
            }]
        );
    }

    #[test]
    fn table_level_unique_combines_column_marker_name_override_and_multiples() {
        let model = DatabaseModel::from_database::<CompositeApp>();
        let widget = table(&model, "widgets");

        // The single-column `#[column(unique)]` marker is hoisted first, then the two table-level
        // `#[unique(...)]` declarations in source order; the first carries an explicit name.
        assert_eq!(
            widget.uniques,
            vec![
                Constraint {
                    name: "uq_widgets_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                },
                Constraint {
                    name: "uq_widget_sku".to_owned(),
                    columns: vec!["tenant_id".to_owned(), "sku".to_owned()],
                },
                Constraint {
                    name: "uq_widgets_tenant_id_label".to_owned(),
                    columns: vec!["tenant_id".to_owned(), "label".to_owned()],
                },
            ]
        );
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
                match_type: None,
                deferrability: None,
                validation: None,
                enforcement: None,
                on_delete: Some(ForeignKeyAction::Cascade),
                on_update: None,
            }]
        );
    }
}
