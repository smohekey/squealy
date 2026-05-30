//! SQL ORM for Rust.

extern crate self as squealy;

mod backend;
mod column;
mod database;
mod expr;
mod foreign_key;
mod index;
pub mod ir;
mod projection;
mod query;
mod schema;
mod table;

pub use backend::{Backend, Connection};
pub use column::{Column, ColumnExpr, ColumnMode, ColumnName, ColumnValue};
pub use database::Database;
pub use expr::{Expr, IntoBindValue, IntoExpr, Order, Predicate, SqlNumber};
pub use foreign_key::ForeignKey;
pub use index::Index;
pub use ir::{
    ArithmeticOp, BindValue, CompareOp, ExprNode, Filter, OrderDirection, OrderNode, PredicateNode,
    Select, SelectColumn, Sort, Source, SourceKind, SourceTarget,
};
pub use projection::{Projectable, ProjectionShape, TableProjection};
pub use query::{SelectBuilder, SelectQuery, build_select};
pub use schema::{DatabaseSchema, DefaultSchema, Schema};
pub use squealy_macros::{Database, Schema, Table};
pub use table::{SchemaTable, Table};
