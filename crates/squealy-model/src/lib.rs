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

pub use package::{
    FORMAT_VERSION, PackageError, from_kdl, read_package, read_package_from, to_kdl, write_package,
    write_package_to,
};
pub use squealy::{
    CheckModel, ColumnModel, Constraint, ConstraintCapabilities, ConstraintEnforcement,
    ConstraintValidation, DatabaseModel, DdlExecutor, DefaultValue, ForeignKeyAction,
    ForeignKeyModel, IndexDirection, IndexMethod, IndexModel, SchemaBackend, SchemaCapabilities,
    SchemaConnect, SchemaIntrospect, SchemaModel, SqlType, TableModel,
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
                if foreign_key.validation.is_some()
                    && !capabilities.constraints.foreign_key_validation
                {
                    return unsupported(
                        &table.name,
                        &foreign_key.name,
                        "foreign key validation metadata",
                    );
                }
                if foreign_key.enforcement.is_some()
                    && !capabilities.constraints.foreign_key_enforcement
                {
                    return unsupported(
                        &table.name,
                        &foreign_key.name,
                        "foreign key enforcement metadata",
                    );
                }
            }
            for check in &table.checks {
                if check.validation.is_some() && !capabilities.constraints.check_validation {
                    return unsupported(&table.name, &check.name, "check validation metadata");
                }
                if check.enforcement.is_some() && !capabilities.constraints.check_enforcement {
                    return unsupported(&table.name, &check.name, "check enforcement metadata");
                }
            }
        }
    }
    Ok(())
}

fn unsupported(table: &str, constraint: &str, feature: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "backend cannot render and introspect {feature} for constraint `{constraint}` on `{table}`"
        ),
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
                        foreign_key_validation: true,
                        foreign_key_enforcement: false,
                        check_validation: false,
                        check_enforcement: true,
                    },
                },
            },
        )
        .expect("reported capabilities should allow rendering");

        assert_eq!(sql, "-- rendered");
    }
}
