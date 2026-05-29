//! SQL ORM for Rust.

extern crate self as squealy;

mod column;
mod database;
mod expr;
mod foreign_key;
mod generator;
mod index;
mod projection;
mod query;
mod schema;
mod table;

pub use column::{Column, ColumnExpr, ColumnMode, ColumnName, ColumnValue};
pub use database::Database;
pub use expr::{Expr, Predicate};
pub use foreign_key::ForeignKey;
pub use generator::Generator;
pub use index::Index;
pub use projection::{Projectable, SelectColumn};
pub use query::{Q, Query, query};
pub use schema::{DatabaseSchema, DefaultSchema, Schema};
pub use squealy_macros::{Database, Schema, Table};
pub use table::{SchemaTable, Table};
