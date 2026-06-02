//! DDL management engine for squealy.
//!
//! The owned, backend-neutral schema model lives in the core crate (so backends can implement
//! [`SchemaBackend`] against it without depending on this engine). This crate adds the operations
//! over that model: `.sqz` package export/import, and create/script/publish deployment orchestration.
//! Heavier dependencies (KDL, zip) are isolated here, away from the query core.
//!
//! See `docs/ddl-management.md` for the design.

#![forbid(unsafe_code)]

mod package;
mod plan;

pub mod diff;

pub use diff::{
    ChangeRisk, ClassifiedDatabaseDiffChange, DatabaseDiff, DatabaseDiffChange, DiffPolicy,
    DiffPolicyError, TableDiffChange, check_diff_policy, diff_models,
};
pub use package::{
    FORMAT_VERSION, PackageError, from_kdl, read_package, read_package_from, to_kdl, write_package,
    write_package_to,
};
pub use plan::{
    ClassifiedDatabasePlanStep, classified_plan_steps, plan_diff, plan_models, plan_step_risk,
    table_plan_step_risk,
};
pub use squealy::{
    CheckModel, ColumnModel, Constraint, ConstraintCapabilities, ConstraintDeferrability,
    ConstraintEnforcement, ConstraintValidation, DatabaseModel, DatabasePlan, DatabasePlanStep,
    DdlExecutor, DefaultValue, ForeignKeyAction, ForeignKeyMatch, ForeignKeyModel,
    IndexCapabilities, IndexCollation, IndexDirection, IndexMethod, IndexModel, IndexNullsOrder,
    IndexOperatorClass, SchemaBackend, SchemaCapabilities, SchemaConnect, SchemaIntrospect,
    SchemaModel, SqlType, TableModel, TablePlanStep,
};

use std::fmt;

use squealy::Database;

/// Renders create-from-scratch DDL for an owned model using the given backend (the "script" /
/// dry-run operation: it produces SQL without touching a database).
pub fn render_create_sql<B: SchemaBackend>(
    model: &DatabaseModel,
    backend: &B,
) -> std::io::Result<String> {
    check_create(model, backend)?;
    let mut buffer = Vec::new();
    backend.render_create(model, &mut buffer)?;
    // SchemaBackend renderers emit UTF-8; treat anything else as a renderer bug.
    Ok(String::from_utf8(buffer).expect("render_create emits valid UTF-8"))
}

/// Renders incremental DDL for a policy-checked [`DatabasePlan`].
pub fn render_plan_sql<B: SchemaBackend>(
    plan: &DatabasePlan,
    backend: &B,
) -> std::io::Result<String> {
    let mut buffer = Vec::new();
    backend.render_plan(plan, &mut buffer)?;
    // SchemaBackend renderers emit UTF-8; treat anything else as a renderer bug.
    Ok(String::from_utf8(buffer).expect("render_plan emits valid UTF-8"))
}

/// Checks whether `backend` can create `model` without rendering or connecting to a database.
///
/// This validates backend capabilities against the neutral model, so callers can fail fast when a
/// package contains metadata the target backend cannot round-trip.
pub fn check_create<B: SchemaBackend>(model: &DatabaseModel, backend: &B) -> std::io::Result<()> {
    validate_capabilities(model, backend.capabilities())
}

/// Renders create-from-scratch DDL straight from a compile-time [`Database`].
///
/// Equivalent to `render_create_sql(&DatabaseModel::from_database::<D>(), backend)`.
pub fn script<D: Database, B: SchemaBackend>(backend: &B) -> std::io::Result<String> {
    render_create_sql(&DatabaseModel::from_database::<D>(), backend)
}

/// An error from [`publish`]: either rendering the DDL or executing it failed.
#[derive(Debug)]
pub enum PublishError<E> {
    Render(std::io::Error),
    Execute(E),
}

impl<E: fmt::Display> fmt::Display for PublishError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::Render(error) => write!(formatter, "failed to render DDL: {error}"),
            PublishError::Execute(error) => write!(formatter, "failed to execute DDL: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for PublishError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PublishError::Render(error) => Some(error),
            PublishError::Execute(error) => Some(error),
        }
    }
}

/// An error from [`plan_from_database`]: either introspection or policy checking failed.
#[derive(Debug)]
pub enum PlanFromDatabaseError<E> {
    Introspect(E),
    Policy(DiffPolicyError),
}

impl<E: fmt::Display> fmt::Display for PlanFromDatabaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanFromDatabaseError::Introspect(error) => {
                write!(formatter, "failed to introspect database: {error}")
            }
            PlanFromDatabaseError::Policy(error) => {
                write!(formatter, "schema plan blocked by policy: {error}")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for PlanFromDatabaseError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanFromDatabaseError::Introspect(error) => Some(error),
            PlanFromDatabaseError::Policy(error) => Some(error),
        }
    }
}

/// Renders create-from-scratch DDL for `model` and executes it against `connection`.
///
/// The backend executes the batch atomically where it supports transactional DDL, so a failed
/// create-from-scratch leaves no partial schema behind.
pub async fn publish<B, C>(
    model: &DatabaseModel,
    backend: &B,
    connection: &mut C,
) -> Result<(), PublishError<C::Error>>
where
    B: SchemaBackend,
    C: DdlExecutor,
{
    let sql = render_create_sql(model, backend).map_err(PublishError::Render)?;
    connection
        .execute_ddl(&sql)
        .await
        .map_err(PublishError::Execute)
}

/// Introspects `connection` and builds an incremental plan from the live model to `desired`.
pub async fn plan_from_database<C>(
    desired: &DatabaseModel,
    connection: &mut C,
    policy: DiffPolicy,
) -> Result<DatabasePlan, PlanFromDatabaseError<C::Error>>
where
    C: SchemaIntrospect,
{
    let actual = introspect(connection)
        .await
        .map_err(PlanFromDatabaseError::Introspect)?;
    plan_models(desired, &actual, policy).map_err(PlanFromDatabaseError::Policy)
}

/// Renders `plan` using `backend` and executes it against `connection`.
pub async fn apply_plan<B, C>(
    plan: &DatabasePlan,
    backend: &B,
    connection: &mut C,
) -> Result<(), PublishError<C::Error>>
where
    B: SchemaBackend,
    C: DdlExecutor,
{
    let sql = render_plan_sql(plan, backend).map_err(PublishError::Render)?;
    connection
        .execute_ddl(&sql)
        .await
        .map_err(PublishError::Execute)
}

/// Publishes create-from-scratch DDL straight from a compile-time [`Database`].
pub async fn publish_database<D, B, C>(
    backend: &B,
    connection: &mut C,
) -> Result<(), PublishError<C::Error>>
where
    D: Database,
    B: SchemaBackend,
    C: DdlExecutor,
{
    publish(&DatabaseModel::from_database::<D>(), backend, connection).await
}

/// Reads the live database schema visible to `connection` into the neutral model.
///
/// Backend crates own the catalog queries and type normalization; the management engine only depends
/// on the shared [`SchemaIntrospect`] contract.
pub async fn introspect<C>(connection: &mut C) -> Result<DatabaseModel, C::Error>
where
    C: SchemaIntrospect,
{
    connection.introspect_database().await
}

fn validate_capabilities(
    model: &DatabaseModel,
    capabilities: SchemaCapabilities,
) -> std::io::Result<()> {
    for schema in &model.schemas {
        for table in &schema.tables {
            for foreign_key in &table.foreign_keys {
                if foreign_key.match_type.is_some()
                    && !capabilities.constraints.foreign_key_match_type
                {
                    return unsupported_constraint(
                        &table.name,
                        &foreign_key.name,
                        "foreign key match metadata",
                    );
                }
                if foreign_key.deferrability.is_some()
                    && !capabilities.constraints.foreign_key_deferrability
                {
                    return unsupported_constraint(
                        &table.name,
                        &foreign_key.name,
                        "foreign key deferrability metadata",
                    );
                }
                if foreign_key.validation.is_some()
                    && !capabilities.constraints.foreign_key_validation
                {
                    return unsupported_constraint(
                        &table.name,
                        &foreign_key.name,
                        "foreign key validation metadata",
                    );
                }
                if foreign_key.enforcement.is_some()
                    && !capabilities.constraints.foreign_key_enforcement
                {
                    return unsupported_constraint(
                        &table.name,
                        &foreign_key.name,
                        "foreign key enforcement metadata",
                    );
                }
            }
            for check in &table.checks {
                if check.validation.is_some() && !capabilities.constraints.check_validation {
                    return unsupported_constraint(
                        &table.name,
                        &check.name,
                        "check validation metadata",
                    );
                }
                if check.enforcement.is_some() && !capabilities.constraints.check_enforcement {
                    return unsupported_constraint(
                        &table.name,
                        &check.name,
                        "check enforcement metadata",
                    );
                }
            }
            for index in &table.indexes {
                if index.predicate.is_some() && !capabilities.indexes.predicates {
                    return unsupported_index(&table.name, &index.name, "partial index predicates");
                }
                if !index.expressions.is_empty() && !capabilities.indexes.expressions {
                    return unsupported_index(&table.name, &index.name, "index expressions");
                }
                if !index.include_columns.is_empty() && !capabilities.indexes.include_columns {
                    return unsupported_index(&table.name, &index.name, "index include columns");
                }
                if !index.nulls.is_empty() && !capabilities.indexes.null_ordering {
                    return unsupported_index(&table.name, &index.name, "index null ordering");
                }
                if !index.collations.is_empty() && !capabilities.indexes.collations {
                    return unsupported_index(
                        &table.name,
                        &index.name,
                        "index collation overrides",
                    );
                }
                if !index.operator_classes.is_empty() && !capabilities.indexes.operator_classes {
                    return unsupported_index(&table.name, &index.name, "index operator classes");
                }
            }
        }
    }
    Ok(())
}

fn unsupported_constraint(table: &str, constraint: &str, feature: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "backend cannot render and introspect {feature} for constraint `{constraint}` on `{table}`"
        ),
    ))
}

fn unsupported_index(table: &str, index: &str, feature: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("backend cannot render and introspect {feature} for index `{index}` on `{table}`"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBackend {
        capabilities: SchemaCapabilities,
    }

    impl SchemaBackend for TestBackend {
        fn capabilities(&self) -> SchemaCapabilities {
            self.capabilities
        }

        fn render_create(
            &self,
            _model: &DatabaseModel,
            writer: &mut impl std::io::Write,
        ) -> std::io::Result<()> {
            writer.write_all(b"-- rendered")
        }
    }

    fn table_with_constraints(foreign_key: ForeignKeyModel, check: CheckModel) -> DatabaseModel {
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "memberships".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys: vec![foreign_key],
                    uniques: vec![],
                    checks: vec![check],
                    indexes: vec![],
                }],
            }],
        }
    }

    fn foreign_key() -> ForeignKeyModel {
        ForeignKeyModel {
            name: "fk_memberships_tenant_id".to_owned(),
            columns: vec!["tenant_id".to_owned()],
            references_schema: None,
            references_table: "tenants".to_owned(),
            references_columns: vec!["id".to_owned()],
            match_type: None,
            deferrability: None,
            validation: None,
            enforcement: None,
            on_delete: None,
            on_update: None,
        }
    }

    fn check() -> CheckModel {
        CheckModel {
            name: "ck_memberships_quota".to_owned(),
            expression: "quota > 0".to_owned(),
            validation: None,
            enforcement: None,
        }
    }

    fn table_with_index(index: IndexModel) -> DatabaseModel {
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "memberships".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![index],
                }],
            }],
        }
    }

    fn index() -> IndexModel {
        IndexModel {
            name: "idx_memberships_tenant_id".to_owned(),
            columns: vec!["tenant_id".to_owned()],
            expressions: vec![],
            include_columns: vec![],
            unique: false,
            method: None,
            directions: vec![],
            nulls: vec![],
            collations: vec![],
            operator_classes: vec![],
            predicate: None,
        }
    }

    #[test]
    fn render_create_rejects_unsupported_constraint_capabilities() {
        let mut foreign_key = foreign_key();
        foreign_key.validation = Some(ConstraintValidation::NotValidated);
        let mut check = check();
        check.enforcement = Some(ConstraintEnforcement::NotEnforced);
        let model = table_with_constraints(foreign_key, check);

        let error = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities::default(),
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error
                .to_string()
                .contains("foreign key validation metadata"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn render_create_rejects_unsupported_foreign_key_shape_capabilities() {
        let mut foreign_key = foreign_key();
        foreign_key.match_type = Some(ForeignKeyMatch::Full);
        foreign_key.deferrability = Some(ConstraintDeferrability::InitiallyDeferred);
        let model = table_with_constraints(foreign_key, check());

        let error = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities::default(),
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("foreign key match metadata"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn render_create_rejects_unsupported_index_capabilities() {
        let mut index = index();
        index.predicate = Some("tenant_id IS NOT NULL".to_owned());
        index.expressions = vec!["lower(name)".to_owned()];
        index.include_columns = vec!["created_at".to_owned()];
        index.nulls = vec![IndexNullsOrder::Last];
        index.collations = vec![IndexCollation {
            position: 0,
            name: "C".to_owned(),
        }];
        index.operator_classes = vec![IndexOperatorClass {
            position: 0,
            name: "text_pattern_ops".to_owned(),
        }];
        let model = table_with_index(index);

        let error = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities::default(),
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("partial index predicates"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn render_create_allows_reported_constraint_capabilities() {
        let mut foreign_key = foreign_key();
        foreign_key.validation = Some(ConstraintValidation::NotValidated);
        let mut check = check();
        check.enforcement = Some(ConstraintEnforcement::NotEnforced);
        let model = table_with_constraints(foreign_key, check);

        let sql = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities {
                    constraints: ConstraintCapabilities {
                        foreign_key_match_type: false,
                        foreign_key_deferrability: false,
                        foreign_key_validation: true,
                        foreign_key_enforcement: false,
                        check_validation: false,
                        check_enforcement: true,
                    },
                    indexes: IndexCapabilities::default(),
                },
            },
        )
        .expect("reported capabilities should allow rendering");

        assert_eq!(sql, "-- rendered");
    }

    #[test]
    fn render_create_allows_reported_index_capabilities() {
        let mut index = index();
        index.predicate = Some("tenant_id IS NOT NULL".to_owned());
        index.expressions = vec!["lower(name)".to_owned()];
        index.include_columns = vec!["created_at".to_owned()];
        index.nulls = vec![IndexNullsOrder::Last];
        index.collations = vec![IndexCollation {
            position: 0,
            name: "C".to_owned(),
        }];
        index.operator_classes = vec![IndexOperatorClass {
            position: 0,
            name: "text_pattern_ops".to_owned(),
        }];
        let model = table_with_index(index);

        let sql = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities {
                    constraints: ConstraintCapabilities::default(),
                    indexes: IndexCapabilities {
                        predicates: true,
                        expressions: true,
                        include_columns: true,
                        null_ordering: true,
                        collations: true,
                        operator_classes: true,
                    },
                },
            },
        )
        .expect("reported capabilities should allow rendering");

        assert_eq!(sql, "-- rendered");
    }
}
