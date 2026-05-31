//! SQL ORM for Rust.
//!
//! Squealy builds SQL queries as typed Rust values. Backends own rendering and execution, while
//! the core crate owns the table metadata, expression operators, and query typestates.
//!
//! Select queries start from a source table and finish with `select`:
//!
//! ```rust,no_run
//! # use squealy::*;
//! # use squealy_test::TestConnection;
//! #
//! # #[derive(Clone, Debug, PartialEq, Table)]
//! # struct User<'scope, C: ColumnMode = ColumnExpr> {
//! #     id: C::Type<'scope, i32>,
//! #     name: C::Type<'scope, String>,
//! # }
//! #
//! # #[derive(Clone, Debug, PartialEq, Table)]
//! # struct Post<'scope, C: ColumnMode = ColumnExpr> {
//! #     id: C::Type<'scope, i32>,
//! #     user_id: C::Type<'scope, i32>,
//! #     title: C::Type<'scope, String>,
//! # }
//! #
//! # let conn = TestConnection;
//! let rows = conn
//!     .from::<User>()
//!     .where_(|user| user.name.equals("Ada"))
//!     .join::<Post>()
//!     .on(|(user,), post| post.user_id.equals(user.id))
//!     .order_by(|(_user, post)| post.id.desc())
//!     .select(|(user, post)| (user.id, post.title))
//!     .fetch();
//! ```
//!
//! Mutations use the same source/destination vocabulary: `to` for insert and update, `from` for
//! delete. Returning mutations use explicit verb names so the final action stays clear.
//!
//! ```rust,no_run
//! # use squealy::*;
//! # use squealy_test::{TestConnection, TestError};
//! #
//! # #[derive(Clone, Debug, PartialEq, Table)]
//! # struct User<'scope, C: ColumnMode = ColumnExpr> {
//! #     #[column(primary_key, auto_increment)]
//! #     id: C::Type<'scope, i32>,
//! #     name: C::Type<'scope, String>,
//! # }
//! #
//! # async fn demo(conn: TestConnection) -> Result<(), TestError> {
//! conn.to::<User>().name("Ada").insert().await?;
//!
//! let created = conn
//!     .to::<User>()
//!     .name("Ada")
//!     .insert_returning(|user| user.id)
//!     .fetch_one()
//!     .await?;
//!
//! conn.to::<User>()
//!     .name("Grace")
//!     .where_(|user| user.name.equals("Ada"))
//!     .update()
//!     .await?;
//!
//! conn.from::<User>()
//!     .where_(|user| user.id.equals(1))
//!     .delete()
//!     .await?;
//! #
//! # _ = created;
//! # Ok(())
//! # }
//! ```
//!
//! Runtime parameters make a query preparable instead of directly executable. Prepared statements
//! keep SQL generation inside the backend and accept typed values at execution time.
//!
//! ```rust,no_run
//! # use squealy::*;
//! # use squealy_test::{TestConnection, TestError};
//! #
//! # #[derive(Clone, Debug, PartialEq, Table)]
//! # struct User<'scope, C: ColumnMode = ColumnExpr> {
//! #     id: C::Type<'scope, i32>,
//! #     name: C::Type<'scope, String>,
//! # }
//! #
//! # async fn demo(conn: TestConnection) -> Result<(), TestError> {
//! let query = conn
//!     .from::<User>()
//!     .where_(|user| user.name.equals(squealy::param::<UserName>()))
//!     .select(|(user,)| user.id);
//! let by_name = query.prepare().await?;
//!
//! let ids = by_name.collect(("Ada",)).await?;
//! #
//! # _ = ids;
//! # Ok(())
//! # }
//! ```
//!
//! Streaming methods such as `fetch` avoid collecting rows up front. Convenience methods like
//! `collect`, `to_sql`, and `collect_params` allocate by design.

extern crate self as squealy;

mod backend;
mod column;
mod connection;
mod database;
mod expr;
mod foreign_key;
mod index;
mod list;
mod projection;
mod query;
mod schema;
mod table;

pub use backend::{Backend, Decode, RowReader};
pub use column::{
    Column, ColumnDefault, ColumnExpr, ColumnMode, ColumnName, ColumnNullableValue, ColumnValue,
};
pub use connection::{Connection, ConnectionWithTransaction, QueryBuilder};
pub use database::Database;
pub use expr::{
    AddExpr, AndPredicate, AnyPredicate, ArithmeticOp, BinaryExprAst, BindValue, BindValueKind,
    ColumnExprAst, ColumnRef, CompareOp, ComparePredicateAst, DivideExpr, EqualsPredicate, Expr,
    ExprAst, ExprKind, ExprVisitor, FloatWidth, GreaterThanOrEqualsPredicate, GreaterThanPredicate,
    IntWidth, IntoBindValue, IntoExpr, IntoNullableBindValue, LessThanOrEqualsPredicate,
    LessThanPredicate, LiteralExprAst, MultiplyExpr, NotEqualsPredicate, NotPredicate, Nullable,
    OrPredicate, Order, OrderDirection, ParamExprAst, Predicate, PredicateAst, PredicateAstVisitor,
    PredicateKind, RuntimeParam, SourceAlias, SqlNumber, SubtractExpr, UIntWidth, param,
};
pub use foreign_key::ForeignKey;
pub use index::Index;
pub use list::{
    BindSink, FixedList, HAppend, HCons, HList, HNil, IntoPreparedParam, MapFixedList,
    NoRuntimeParams, PreparedParamValues, PushBack, ToTuple, TupleAppend, TupleConcat,
};
pub use projection::{Maybe, Projectable, ProjectionShape, ProjectionVisitor, TableProjection};
pub use query::{
    AllRows, AssignmentValueNode, AssignmentValueRef, ColumnKey, DeleteQuery, DeleteSourceAst,
    DeleteSourceQuery, ExecutableDeleteQuery, ExecutableInsertQuery, ExecutableSelectQuery,
    ExecutableUpdateQuery, From, InnerJoinSource, InsertAssignment, InsertAssignmentNode,
    InsertAssignments, InsertQuery, IntoAssignmentValue, IntoNullableAssignmentValue, Join,
    JoinTarget, LeftJoin, LeftJoinSource, LeftJoinTarget, Limited, MutationFiltered,
    MutationUnfiltered, NoSources, Offset, OrderBy, PredicateNodes, PredicateVisitor,
    PreparableDeleteQuery, PreparableInsertQuery, PreparableSelectQuery, PreparableUpdateQuery,
    PreparedMutationQuery, PreparedSelectQuery, ReturningProjection, RootSource, RowsAffected,
    RuntimeAssignmentValue, SelectAst, SelectQuery, SelectSink, Selected, SourceQuery, SourceSpec,
    StaticAssignmentValue, UpdateAssignment, UpdateAssignmentNode, UpdateAssignments, UpdateQuery,
    Where,
};
pub use schema::{DatabaseSchema, DefaultSchema, Schema};
pub use squealy_macros::{Database, Schema, Table};
pub use table::{InsertableTable, SchemaTable, Table, UpdateableTable, WriteableTable};
