//! SQL ORM for Rust.

extern crate self as squealy;

mod backend;
mod column;
mod database;
mod expr;
mod foreign_key;
mod index;
mod projection;
mod query;
mod schema;
mod table;

pub use backend::{Backend, Connection};
pub use column::{Column, ColumnExpr, ColumnMode, ColumnName, ColumnValue};
pub use database::Database;
pub use expr::{
    ArithmeticOp, BindValue, CompareOp, Expr, ExprNode, IntoBindValue, IntoExpr, Order,
    OrderDirection, OrderNode, Predicate, PredicateNode, SqlNumber,
};
pub use foreign_key::ForeignKey;
pub use index::Index;
pub use projection::{Projectable, ProjectionShape, SelectColumn, TableProjection};
pub use query::{Filter, Q, Query, Select, Sort, Source, SourceKind, SourceTarget, build_select};
pub use schema::{DatabaseSchema, DefaultSchema, Schema};
pub use squealy_macros::{Database, Schema, Table};
pub use table::{SchemaTable, Table};
