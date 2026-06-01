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
    CheckModel, ColumnModel, Constraint, DatabaseModel, DefaultValue, ForeignKeyModel, IndexModel,
    SchemaBackend, SchemaModel, SqlType, TableModel,
};
