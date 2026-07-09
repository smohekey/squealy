//! SQL ORM for Rust.
//!
//! ## Why Squealy?
//!
//! Squealy is a typed query builder and schema metadata layer for Rust applications that want SQL
//! without treating SQL as unstructured strings. Table derives turn your Rust row types into typed
//! column expressions, row decoding shapes, DDL metadata, and mutation builders. Query methods then
//! compose those generated types into a type-level query AST: sources, joins, filters, projections,
//! ordering, mutation assignments, and runtime parameter shapes are all carried by Rust types.
//!
//! The core crate deliberately stops at describing queries and schema. Backend crates, such as a
//! PostgreSQL backend, own SQL rendering, bind handling, preparation, execution, streaming rows,
//! and transaction behavior. That split lets each backend decide how to turn the typed AST into the
//! best SQL for that database, while the shared builder API keeps user code backend-shaped rather
//! than string-shaped.
//!
//! Runtime values are explicit. Literal values can be captured directly in a concrete query, while
//! [`param`] creates typed runtime parameters that must be prepared before execution. Streaming is
//! the default result model through `fetch`; allocating helpers such as `collect`, `to_sql`, and
//! `collect_params` are convenience APIs for callers that choose them.
//!
//! ## Model your database with derives
//!
//! Start by deriving [`Table`] for each row type. Table structs currently use this shape:
//!
//! - a lifetime named `'scope`
//! - a column mode parameter `C: ColumnMode = ColumnExpr`
//! - fields typed as `C::Type<'scope, Value>`
//!
//! ```rust
//! use squealy::*;
//!
//! #[derive(Clone, Debug, PartialEq, Table)]
//! #[schema(Public)]
//! struct User<'scope, C: ColumnMode = ColumnExpr> {
//!     #[column(primary_key, auto_increment)]
//!     id: C::Type<'scope, i32>,
//!
//!     #[column(index, default = value("anonymous"))]
//!     name: C::Type<'scope, String>,
//! }
//!
//! #[derive(Clone, Debug, PartialEq, Table)]
//! #[schema(Public)]
//! struct Post<'scope, C: ColumnMode = ColumnExpr> {
//!     #[column(primary_key, auto_increment)]
//!     id: C::Type<'scope, i32>,
//!
//!     #[column(index, references(User::id, on_delete = "cascade"))]
//!     user_id: C::Type<'scope, i32>,
//!
//!     title: C::Type<'scope, String>,
//! }
//!
//! #[derive(Schema)]
//! struct Public {
//!     users: User<'static, ColumnName>,
//!     posts: Post<'static, ColumnName>,
//! }
//!
//! #[derive(Database)]
//! struct AppDatabase {
//!     public: Public,
//! }
//! ```
//!
//! The derive generates table metadata, typed expression projections, row decoding shapes, and a
//! write builder for `conn.to::<Table>()`. It also generates a marker type for each column by
//! combining the table and field names: `User::id` becomes `UserId`, `Post::title` becomes
//! `PostTitle`, and so on. Those marker types are useful when declaring runtime parameters with
//! [`param`].
//!
//! Nullability is declared in the column type: a `C::Type<'scope, Option<T>>` field is a nullable
//! column with value type `T` (mapping to a `NULL`-able DDL column and decoding as `Option<T>`),
//! while `C::Type<'scope, T>` is `NOT NULL`. There is no `#[column(nullable)]` attribute. The
//! `Option<…>` must be written literally in the field type (a type alias to `Option` is not seen).
//!
//! Common column attributes include:
//!
//! - `primary_key`, `auto_increment`, `index`, and `unique`
//! - `generated`, `insert = false`, and `update = false`
//! - `default = value(...)`, `default = current_timestamp`, `default = current_date`,
//!   `default = current_time`, and `default_raw = "..."`
//! - `check = "..."`, plus `db_type = "..."` as a raw backend-specific DDL type override
//! - `references(OtherTable::column, on_delete = "...", on_update = "...")`
//!
//! If `db_type` is omitted, the field's Rust value type must implement [`HasColumnType`].
//! Primitive Rust types already do, and backend crates map those logical types to appropriate
//! database DDL. For example, the PostgreSQL backend renders `i32` as `integer` and `String` as
//! `text`. Use `db_type` only when you need an explicit backend-specific escape hatch such as
//! `varchar(64)`, `jsonb`, or a domain type. If a custom field type does not implement
//! [`HasColumnType`] and does not provide `db_type`, the table derive fails to compile. A `db_type`
//! column whose value type is a bare type (not a `#[derive(ColumnType)]` newtype) must still declare
//! its nullability via `squealy::impl_non_null_column!(MyType);`.
//!
//! Enabling the `uuid` feature maps a bare `uuid::Uuid` field to a `uuid` column (no `db_type`
//! override needed) and lets a `Uuid` value be used directly in the query builder — as a predicate
//! operand (`col.equals(id)`) and as a write-builder setter (`.id(id)`). It also covers nullable UUID
//! columns (`Option<uuid::Uuid>`) and left-joined UUID tables. Pair it with a backend that implements
//! `Encode`/`Decode` for `uuid::Uuid` (the PostgreSQL backend's own `uuid` feature does, and turns on
//! `squealy/uuid` for you).
//!
//! Timestamp columns are available behind feature flags: `systemtime` maps `std::time::SystemTime`
//! to a `timestamptz` column with no extra dependency, while `time` and `chrono` map
//! `time::OffsetDateTime` and `chrono::DateTime<Utc>` respectively. Each works in both non-null and
//! nullable (`Option<…>`) columns and in the query builder, paired with a backend that
//! enables the matching feature (the PostgreSQL backend turns on the core feature for you).
//!
//! For newtype wrappers, derive `ColumnType` on the wrapper. Single-field tuple structs and
//! single-field named structs are transparent by default, so the wrapper uses the same database
//! type, bind conversion, row decoding, and literal expression support as its inner value. Use
//! `#[column_type(db_type = "...")]` when the wrapper should still bind/decode transparently but use
//! a backend-specific database type:
//!
//! ```rust
//! use squealy::*;
//!
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
//! pub struct UserId(i32);
//!
//! #[derive(Clone, Debug, PartialEq, ColumnType)]
//! #[column_type(db_type = "jsonb")]
//! pub struct JsonPayload(String);
//!
//! #[derive(Clone, Debug, PartialEq, Table)]
//! struct Event<'scope, C: ColumnMode = ColumnExpr> {
//!     id: C::Type<'scope, UserId>,
//!     payload: C::Type<'scope, JsonPayload>,
//! }
//! ```
//!
//! `#[schema(Type)]` attaches a table to a schema namespace. `#[derive(Schema)]` lists the tables
//! in that namespace, and `#[derive(Database)]` lists schemas for DDL/backends that want database
//! metadata.
//!
//! Constraints spanning more than one column are declared as table-level attributes on the struct:
//!
//! - `#[primary_key(columns = [a, b])]` for a composite primary key,
//! - `#[unique(columns = [a, b], name = "...")]` for a composite unique constraint (repeatable;
//!   `name` is optional and defaults to `uq_<table>_<columns>`),
//! - `#[index(columns = [a, b], unique, name = "...")]` for a multi-column index.
//!
//! The single-column forms `#[column(primary_key)]`, `#[column(unique)]`, and `#[column(index)]`
//! remain available for one-column constraints.
//!
//! ### Partial (filtered) unique indexes
//!
//! A `where = |row| ...` clause turns a unique into a *partial unique index*
//! (`CREATE UNIQUE INDEX ... WHERE <predicate>`) — the standard soft-delete idiom, where a slug or
//! email is unique only among live rows and reusable once a row is soft-deleted:
//!
//! ```rust
//! # use squealy::*;
//! #[derive(Table)]
//! struct Organization<'scope, C: ColumnMode = ColumnExpr> {
//!     #[column(primary_key)]
//!     id: C::Type<'scope, i32>,
//!     #[column(unique, where = |row| row.deleted_at.is_null())]
//!     slug: C::Type<'scope, String>,
//!     deleted_at: C::Type<'scope, Option<i64>>,
//! }
//! ```
//!
//! The same `where = ...` is accepted on the table-level `#[unique(columns = [..], where = ...)]`
//! (composite partial uniques) and `#[index(columns = [..], where = ...)]`. The predicate is a
//! typed expression over the table's columns — the same builders used in `where_` (`.is_null()`,
//! `.is_not_null()`, comparisons, `.and()` / `.or()` / `.not_()`) — so a typo in a column name is a
//! compile error, not bad SQL. Operands may be other columns or scalar value literals
//! (`col.equals(0)`, `col.equals("active")`, `col.greater_than(10)`); literals of the integer,
//! float, `bool`, and `String` value types render inline. Operands whose SQL form is
//! backend-specific — a timestamp/`uuid` literal, a runtime bind param, or an arithmetic
//! sub-expression — are not supported and fail to compile. Partial indexes are PostgreSQL-only;
//! MySQL rejects them.
//!
//! ## Stream rows from select queries
//!
//! Select queries start from a source table and finish with `select`:
//!
//! ```rust,no_run
//! # use squealy::*;
//! # use squealy_test::TestConnection;
//! # use std::future::poll_fn;
//! # use std::pin::pin;
//! # use futures_core::Stream;
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
//! # async fn demo(conn: TestConnection) -> Result<(), squealy_test::TestError> {
//! let query = conn
//!     .from::<User>()
//!     .where_(|user| user.name.equals("Ada"))
//!     .join::<Post>()
//!     .on(|(user,), post| post.user_id.equals(user.id))
//!     .order_by(|(_user, post)| post.id.desc())
//!     .select(|(user, post)| (user.id, post.title));
//!
//! let mut rows = pin!(query.fetch());
//! while let Some(row) = poll_fn(|cx| rows.as_mut().poll_next(cx)).await {
//!     let (user_id, title) = row?;
//!     // Process each row as it arrives instead of collecting every row first.
//!     # _ = (user_id, title);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! For smaller result sets where allocation is acceptable, use `collect()`:
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
//! # let conn = TestConnection;
//! let rows = conn
//!     .from::<User>()
//!     .where_(|user| user.name.equals("Ada"))
//!     .select(|(user,)| (user.id, user.name))
//!     .collect()
//!     .await?;
//! # _ = rows;
//! # Ok(())
//! # }
//! ```
//!
//! Projecting an `Option<T>` (nullable) column yields `Option<T>`, so a SQL `NULL` decodes instead of
//! erroring — the same way selecting the whole row decodes nullable fields. Non-null columns project
//! as their bare value type.
//!
//! ## Write data with type-checked mutations
//!
//! Mutations use explicit direction words: `to` for insert and update destinations, `from` for
//! delete sources. Returning mutations use explicit verb names such as `insert_returning` and
//! `update_returning` so the final action stays clear.
//!
//! ### Insert rows
//!
//! Use `conn.to::<Table>()` when assigning columns one at a time through the table-derived field
//! setters. The table derive skips non-insertable columns and only exposes setters for columns that
//! may be inserted.
//!
//! ```rust,no_run
//! # use squealy::*;
//! # use squealy_test::{TestConnection, TestError};
//! #
//! # #[derive(Clone, Debug, PartialEq, Table)]
//! # struct User<'scope, C: ColumnMode = ColumnExpr> {
//! #     #[column(primary_key, auto_increment)]
//! #     id: C::Type<'scope, i32>,
//! #     #[column(default = value("anonymous"))]
//! #     name: C::Type<'scope, String>,
//! #     login_count: C::Type<'scope, i32>,
//! # }
//! #
//! # async fn demo(conn: TestConnection) -> Result<(), TestError> {
//! conn.to::<User>().name("Ada").login_count(0).insert().await?;
//!
//! let created = conn
//!     .to::<User>()
//!     .name("Ada")
//!     .login_count(0)
//!     .insert_returning(|user| user.id)
//!     .fetch_one()
//!     .await?;
//! #
//! # _ = created;
//! # Ok(())
//! # }
//! ```
//!
//! Use `conn.to_columns::<Table, Columns>()` when you want to name the target column set up front.
//! `Columns` is a tuple of marker types generated by `#[derive(Table)]`, such as `UserName`, and
//! each `.row(...)` call must provide values in that same order. This form supports fixed-shape
//! multi-row inserts. Use `default()` where a row should ask the database to apply the column
//! default instead of binding a value.
//!
//! ```rust,no_run
//! # use squealy::*;
//! # use squealy_test::{TestConnection, TestError};
//! #
//! # #[derive(Clone, Debug, PartialEq, Table)]
//! # struct User<'scope, C: ColumnMode = ColumnExpr> {
//! #     #[column(primary_key, auto_increment)]
//! #     id: C::Type<'scope, i32>,
//! #     #[column(default = value("anonymous"))]
//! #     name: C::Type<'scope, String>,
//! #     login_count: C::Type<'scope, i32>,
//! # }
//! #
//! # async fn demo(conn: TestConnection) -> Result<(), TestError> {
//! conn.to_columns::<User, (UserName, UserLoginCount)>()
//!     .row(("Ada", 0))
//!     .row((default(), 0))
//!     .insert()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! ### Update rows
//!
//! Use `conn.to::<Table>()` for ordinary updates with table-derived field setters. Updates must be
//! filtered with `where_` or explicitly marked with `all()` before they can execute.
//!
//! ```rust,no_run
//! # use squealy::*;
//! # use squealy_test::{TestConnection, TestError};
//! #
//! # #[derive(Clone, Debug, PartialEq, Table)]
//! # struct User<'scope, C: ColumnMode = ColumnExpr> {
//! #     #[column(primary_key, auto_increment)]
//! #     id: C::Type<'scope, i32>,
//! #     #[column(default = value("anonymous"))]
//! #     name: C::Type<'scope, String>,
//! #     login_count: C::Type<'scope, i32>,
//! # }
//! #
//! # async fn demo(conn: TestConnection) -> Result<(), TestError> {
//! conn.to::<User>()
//!     .name("Grace")
//!     .where_(|user| user.name.equals("Ada"))
//!     .update()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! `to_columns(...).set(|table| ...)` is the explicit-column update form. The closure receives
//! scoped table expressions, so assignments can reference existing column values as part of the
//! update expression. `default()` can also be used in update assignments.
//!
//! ```rust,no_run
//! # use squealy::*;
//! # use squealy_test::{TestConnection, TestError};
//! #
//! # #[derive(Clone, Debug, PartialEq, Table)]
//! # struct User<'scope, C: ColumnMode = ColumnExpr> {
//! #     #[column(primary_key, auto_increment)]
//! #     id: C::Type<'scope, i32>,
//! #     #[column(default = value("anonymous"))]
//! #     name: C::Type<'scope, String>,
//! #     login_count: C::Type<'scope, i32>,
//! # }
//! #
//! # async fn demo(conn: TestConnection) -> Result<(), TestError> {
//! let row = conn
//!     .to_columns::<User, (UserLoginCount, UserName)>()
//!     .set(|user| (user.login_count + 1, default()))
//!     .where_(|user| user.id.equals(1))
//!     .update_returning(|user| (user.id, user.login_count))
//!     .fetch_one()
//!     .await?;
//! #
//! # _ = row;
//! # Ok(())
//! # }
//! ```
//!
//! ### Delete rows
//!
//! Deletes start with `from::<Table>()`, then use the same typed predicates as selects. Like
//! updates, deletes must be filtered with `where_` or explicitly marked with `all()` before
//! execution.
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
//! conn.from::<User>()
//!     .where_(|user| user.id.equals(1))
//!     .delete()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Prepare runtime-parameterized queries
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

#![forbid(unsafe_code)]

extern crate self as squealy;

mod backend;
mod column;
mod connection;
mod database;
mod dialect;
mod expr;
mod foreign_key;
mod index;
mod list;
mod model;
mod plan;
mod projection;
mod query;
// The shared SQL query renderer. Public so backend crates can call its entry points, but hidden from
// the documented API — it is an implementation detail backends drive via their `Dialect`.
mod cte;
#[doc(hidden)]
pub mod render;
mod schema;
mod table;
mod view;
pub mod view_render;

pub use backend::{
    Backend, ConstraintCapabilities, DdlExecutor, Decode, DecodeNullable, Encode,
    IndexCapabilities, ParamWriter, RowReader, SchemaBackend, SchemaCapabilities, SchemaConnect,
    SchemaIntrospect, SchemaMetadataStore, SchemaPublishHistoryStore, SchemaPublishRecord,
    SchemaRefactorStore, SupportsDateTrunc, SupportsExtract, SupportsFullJoin, SupportsNamedWindow,
    SupportsReturning,
};
pub use column::{
    Column, ColumnDefault, ColumnExpr, ColumnMode, ColumnName, ColumnNullability,
    ColumnNullableValue, ColumnType, ColumnValue, HasColumnType,
};
pub use connection::{Connection, ConnectionWithTransaction, QueryBuilder};
pub use cte::{
    CteBody, CteDef, CteDefinition, RecursiveBody, RecursiveCteDefinition, RecursiveUnion,
    SchemaCte, cte_definition_dependencies, cte_definition_model, recursive_cte_definition_body,
    recursive_cte_definition_dependencies,
};
pub use database::Database;
pub use dialect::{DeleteUsingStyle, Dialect, SetOperandStyle, UpdateFromStyle};
pub use expr::{
    AddExpr, AggregateExprAst, AggregateScalar, AndPredicate, AnyPredicate, AppendArm,
    AppendCoalesceArg, AppendOrder, AppendPartition, AppendSimpleArm, AvgExpr, BetweenPredicate,
    BetweenPredicateAst, BinaryExprAst, BoolTestPredicate, BoolTestPredicateAst, CaseArmsParams,
    CaseBuilder, CaseExprAst, CaseMaybeNull, CaseNil, CaseNonNull, CaseNull, CaseNullAnd,
    CaseNullOr, CaseWhen, CoalesceArg, CoalesceArgsParams, CoalesceBuilder, CoalesceExprAst,
    CoalesceNil, ColumnBetween, ColumnCaseNull, ColumnComparison, ColumnExprAst, ColumnLike,
    ColumnRef, ComparePredicateAst, ConcatExprAst, CountExpr, CurrentRow, DateTruncExprAst,
    DdlExprAst, DdlPredicateAst, DdlSqlLiteral, DivideExpr, EqualsPredicate, ExistsPredicate,
    ExistsPredicateAst, Expr, ExprAst, ExprKind, ExprVisitor, ExtractAtExprAst, ExtractExprAst,
    ExtractSecondExprAst, Following, FrameEnd, FrameStart, GreaterThanOrEqualsPredicate,
    GreaterThanPredicate, InPredicate, InPredicateAst, InSubqueryPredicateAst, IntoExpr,
    IntoWindowNullable, IsNotNullPredicate, IsNullPredicate, KindNullability, LagArgsAst,
    LessThanOrEqualsPredicate, LessThanPredicate, LikePredicate, LikePredicateAst, LiteralExprAst,
    MaxExpr, MinExpr, MultiplyExpr, NamedWindowExprAst, NoElse, NoFrame, NonAggregateAst,
    NonAggregatePredicate, NonEmptyArms, NonWindowAst, NotEqualsPredicate, NotPredicate,
    NowExprAst, NullCheckPredicateAst, Nullable, NullableExpr, NullifExprAst, OrPredicate, Order,
    ParamExprAst, PendingWindow, Preceding, Predicate, PredicateAst, PredicateAstVisitor,
    PredicateKind, ProjectionParams, RenderAst, RenderCaseArms, RenderCoalesceArgs,
    RenderPredicateAst, RenderSimpleCaseArms, RenderWindowList, ReturnableProjection, RuntimeParam,
    SameValue, ScalarNullable, ScalarProjection, ScalarSubqueryExprAst, SimpleCaseArmsParams,
    SimpleCaseBuilder, SimpleCaseExprAst, SimpleCaseNil, SimpleCaseWhen, SourceAlias, SqlDivide,
    SqlNumber, SqlSum, SqlText, SubstringExprAst, SubtractExpr, SumExpr, TimestampKind,
    UnaryFnExprAst, UnaryStringFunc, UnboundedFollowing, UnboundedPreceding, Window, WindowExprAst,
    WindowFrame, WindowListParams, WindowNil, WindowNoArg, WindowOperand, WindowOrder,
    WindowPartition, WindowRef, case, case_of, coalesce, current_row, date_trunc, date_trunc_at,
    dense_rank, exists, extract, extract_at, extract_second, following, lag, lead, length, lower,
    named_window_alias, not_exists, now, ntile, nullif, param, preceding, rank,
    render_ddl_predicate, row_number, scalar_subquery, substring, trim, unbounded_following,
    unbounded_preceding, upper,
};
pub use expr::{
    AggregateNeedsGroupBy, AggregateProjection, AggregateTerm, Aggregated, AstProjectionClass,
    ColumnFree, ColumnTerm, CombineColumns, CombineTerm, ConstantTerm, DistinctOrderGate,
    ExprColumns, ExtendOrderClass, Grouped, HasBareColumn, HavingTransition, IntoKindList,
    IsDistinct, NotDistinct, NotOuterJoined, OrderAggregate, OrderCompatibleWith,
    OrderKeysInProjection, OrderMixed, OrderNone, OrderNullsTerm, OrderScalar, OuterJoined,
    PredicateColumns, ProjectionClass, ProjectionColumns, RowLockSelectValid, RowLocked,
    RowUnlocked, TermProjectionClass, Ungrouped, ValidSelect,
};
pub use foreign_key::ForeignKey;
pub use index::Index;
pub use list::{
    Contains, FixedList, HAppend, HCons, HList, HNil, Here, IntoNullableExprs, IntoPreparedParam,
    MapFixedList, NoRuntimeParams, PreparedParamValues, PushBack, RequiredCol, RequiredCovered,
    There, ToTuple, TupleAppend, TupleConcat,
};
pub use model::{DatabaseModel, ViewDef, table_from_dyn};
pub use plan::{DatabasePlan, DatabasePlanStep, TablePlanStep};
pub use projection::{
    Maybe, Projectable, ProjectionShape, ProjectionVisitor, QuerySource, RenderProjectable,
    TableProjection,
};
pub use query::{
    AllRows, AssignmentNode, AssignmentValueNode, AssignmentValueVisitor, AssignmentVisitor,
    ColumnKey, ConflictAction, ConflictClause, ConflictTarget, CrossJoinSource,
    DefaultAssignmentValue, DefaultValueNode, DeleteQuery, DeleteSourceAst, DeleteSourceQuery,
    DeleteUsingBuilder, DeleteUsingQuery, Distinct, ExecutableDeleteQuery,
    ExecutableDeleteUsingQuery, ExecutableInsertQuery, ExecutableSelectQuery,
    ExecutableSetSelectQuery, ExecutableUpdateFromQuery, ExecutableUpdateQuery,
    ExplicitUpdateBuilder, ExprAssignmentValue, From, FullJoinSource, FullJoinTarget,
    InnerJoinSource, InsertAssignment, InsertAssignmentNode, InsertAssignments, InsertColumnKey,
    InsertColumnNullability, InsertColumnValues, InsertQuery, InsertReady, InsertRow,
    InsertRowVisitor, InsertRows, InsertRowsBuilder, InsertSelectColumns,
    InsertSelectRowCompatible, IntoAssignmentValue, IntoInsertColumnValue, IntoInsertSelect,
    IntoNullableAssignmentValue, IntoUpdateColumnValue, IsNullable, Join, JoinTarget, LeftJoin,
    LeftJoinSource, LeftJoinTarget, Limited, Locked, MutationFiltered, MutationUnfiltered,
    NoSources, NonEmptyInsertRows, NonNullableColumn, NullableColumn, Offset, OnConflict,
    OnConflictQueryBuilder, OrderBy, OuterJoin, PredicateNodes, PredicateVisitor,
    PreparableDeleteQuery, PreparableInsertQuery, PreparableSelectQuery, PreparableSetSelectQuery,
    PreparableUpdateQuery, PreparedMutationQuery, PreparedSelectQuery, RecursiveSelf,
    RenderAssignment, RenderAssignmentValue, RenderInsertAssignments, RenderInsertRows,
    RenderPredicateNodes, RenderSelectAst, RenderSourceSpec, RenderSubquery,
    RenderUpdateAssignments, RendersRowLock, ReturningProjection, RightJoinSource, RightJoinTarget,
    RootSource, RowLock, RowsAffected, RuntimeAssignmentValue, ScalarSubquery, SelectAst,
    SelectQuery, SelectSink, Selected, SetArm, SetGroup, SetLeaf, SetNode, SetOp, SetOperand,
    SetOperations, SetOrder, SetSelectModifiers, SetTail, SourceQuery, SourceSpec,
    StaticAssignmentValue, Subqueries, Subquery, SubquerySelect, SupportsColumnlessUpsert,
    SupportsDefaultKeyword, SupportsIntersectExceptAll, ToColumns, UpdateAssignment,
    UpdateAssignmentNode, UpdateAssignments, UpdateColumnKey, UpdateColumnValues,
    UpdateFromBuilder, UpdateFromColumns, UpdateFromQuery, UpdateQuery, Upsert, ValidUpsertColumns,
    Where, WindowBy, WindowScope, assert_distinct_insert_select_columns, default,
};
pub use schema::{DatabaseSchema, DefaultSchema, Schema};
pub use squealy_ir::{
    AggregateFunc, ArithmeticOp, CaseArm, CheckModel, ColumnModel, CompareOp, Constraint,
    ConstraintDeferrability, ConstraintEnforcement, ConstraintValidation, CteModel, DateField,
    DefaultValue, ExprNode, ForeignKeyAction, ForeignKeyMatch, ForeignKeyModel, FrameBound,
    FrameMode, FrameSpec, GeneratedColumnModel, GeneratedStorage, IdentityMode, IdentityModel,
    IndexCollation, IndexDirection, IndexMethod, IndexModel, IndexNullsOrder, IndexOperatorClass,
    JoinItem, JoinKind, LogicalOp, OrderDirection, OrderItem, OrderNulls, ProjectionItem,
    ScalarFunc, SchemaModel, SourceItem, SourceRef, SqlType, TableModel, ViewBody, ViewColumnModel,
    ViewModel, ViewQueryModel, ViewSetOp, WindowFunc, WindowOrderTerm,
    fold_like_case_insensitivity, normalize_expr,
};
pub use squealy_macros::{CTE, ColumnType, Database, RecursiveCTE, Schema, Table, View};
pub use table::{
    InsertableTable, SchemaTable, Table, TablePrimaryKey, TableUnique, UpdateableTable,
    WriteableTable,
};
pub use view::{ModelBackend, ModelConn, SchemaView, ViewDefinition, ViewSelect};
#[doc(hidden)]
pub use view::{build_ddl_predicate, lower_view, view_definition_model};
pub use view_render::{
    ordered_views, render_create_view, render_cte_body, render_drop_view,
    render_recursive_cte_body, render_scalar_expr,
};
