//! MySQL schema-management backend for squealy.
//!
//! This crate is deliberately **schema-only** (no query backend): it implements the DDL-management
//! traits (`SchemaBackend` now; `SchemaConnect` / `DdlExecutor` / introspection to come) against the
//! core `DatabaseModel`. Its purpose is partly to keep the crate boundaries honest — a second backend
//! that renders a different dialect (backtick quoting, `AUTO_INCREMENT`, unsigned integers,
//! `VARCHAR`-backed strings) without touching core or the model.

#![forbid(unsafe_code)]

use std::io::Write;

use squealy::{DatabaseModel, SchemaBackend};

mod sql;

/// The MySQL schema backend marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mysql;

impl SchemaBackend for Mysql {
    fn render_create(&self, model: &DatabaseModel, writer: &mut impl Write) -> std::io::Result<()> {
        sql::write_database(model, writer)
    }
}
