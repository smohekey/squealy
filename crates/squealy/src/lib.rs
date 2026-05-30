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

pub use backend::{Backend, Connection, Decode, RowReader, TransactionalConnection};
pub use column::{
    Column, ColumnDefault, ColumnExpr, ColumnMode, ColumnName, ColumnNullableValue, ColumnValue,
};
pub use database::Database;
pub use expr::{
    AddExpr, ColumnRef, DivideExpr, Expr, ExprKind, IntoBindValue, IntoExpr, IntoNullableBindValue,
    MultiplyExpr, Nullable, Order, Predicate, SqlNumber, SubtractExpr,
};
pub use foreign_key::ForeignKey;
pub use index::Index;
pub use ir::{
    ArithmeticOp, BindValue, BindValueKind, CompareOp, Delete, ExprNode, Filter, FloatWidth,
    Insert, InsertColumn, IntWidth, OrderDirection, OrderNode, PredicateNode, Select, SelectColumn,
    Sort, Source, SourceKind, SourceTarget, UIntWidth, Update, UpdateColumn,
};
pub use projection::{Maybe, Projectable, ProjectionShape, TableProjection};
pub use query::{
    DeleteBuilder, DeleteQuery, InsertQuery, MutationFiltered, MutationUnfiltered, Returning,
    ReturningProjection, RowsAffected, SelectBuilder, SelectQuery, UpdateQuery, build_delete,
    build_delete_builder, build_delete_returning, build_insert, build_insert_returning,
    build_select, build_update, build_update_returning,
};
pub use schema::{DatabaseSchema, DefaultSchema, Schema};
pub use squealy_macros::{Database, Schema, Table};
pub use table::{InsertableTable, SchemaTable, Table, UpdateableTable};
