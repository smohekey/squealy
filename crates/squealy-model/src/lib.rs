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
    CheckModel, ColumnModel, Constraint, DatabaseModel, DdlExecutor, DefaultValue,
    ForeignKeyAction, ForeignKeyModel, IndexDirection, IndexMethod, IndexModel, SchemaBackend,
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
    let mut buffer = Vec::new();
    backend.render_create(model, &mut buffer)?;
    // SchemaBackend renderers emit UTF-8; treat anything else as a renderer bug.
    Ok(String::from_utf8(buffer).expect("render_create emits valid UTF-8"))
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
