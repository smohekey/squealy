//! SQLite backend for squealy.
//!
//! This is the first slice of the SQLite backend: **DDL rendering only**. It renders
//! create-from-scratch SQLite DDL for a [`DatabaseModel`] (and view definitions) in SQLite's dialect
//! — double-quoted identifiers, type affinities (`INTEGER`/`REAL`/`TEXT`/`BLOB`/`NUMERIC`),
//! `INTEGER PRIMARY KEY AUTOINCREMENT` identity, and **inline** foreign keys (SQLite cannot
//! `ALTER TABLE … ADD CONSTRAINT`).
//!
//! The query runtime (codec, `Backend`, connection/execution via `tokio-rusqlite`) and introspection
//! land in later slices. Incremental plan rendering ([`SchemaBackend::render_plan`]) is not yet
//! supported: SQLite's `ALTER TABLE` only adds/drops/renames columns and renames tables, so most
//! changes need the "create new table, copy, drop, rename" rebuild — its own future slice.

#![forbid(unsafe_code)]

use std::io::{self, Write};

use squealy::{DatabaseModel, DatabasePlan, SchemaBackend};

mod sql;

/// The SQLite schema backend marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sqlite;

impl SchemaBackend for Sqlite {
    fn render_create(&self, model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
        sql::write_database(model, writer)
    }

    fn render_plan(&self, _plan: &DatabasePlan, _writer: &mut impl Write) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SQLite incremental schema plan rendering is not supported yet: SQLite's ALTER TABLE \
             cannot change a column's type or add/drop most constraints, so changes require a \
             create-copy-drop-rename table rebuild (a future slice)",
        ))
    }
}
