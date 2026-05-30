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
pub use column::{Column, ColumnExpr, ColumnMode, ColumnName, ColumnNullableValue, ColumnValue};
pub use database::Database;
pub use expr::{
    AddExpr, ColumnRef, DivideExpr, Expr, ExprKind, IntoBindValue, IntoExpr, MultiplyExpr,
    Nullable, Order, Predicate, SqlNumber, SubtractExpr,
};
pub use foreign_key::ForeignKey;
pub use index::Index;
pub use ir::{
    ArithmeticOp, BindValue, CompareOp, Delete, ExprNode, Filter, Insert, InsertColumn,
    OrderDirection, OrderNode, PredicateNode, Select, SelectColumn, Sort, Source, SourceKind,
    SourceTarget,
};
pub use projection::{Maybe, Projectable, ProjectionShape, TableProjection};
pub use query::{
    DeleteBuilder, DeleteQuery, InsertQuery, Returning, ReturningProjection, SelectBuilder,
    SelectQuery, build_delete, build_insert, build_select,
};
pub use schema::{DatabaseSchema, DefaultSchema, Schema};
pub use squealy_macros::{Database, Schema, Table};
pub use table::{InsertableTable, SchemaTable, Table};
