#![forbid(unsafe_code)]

use std::fmt;

use squealy::{
	Backend, Connection, ConnectionWithTransaction, Decode, InsertableTable, Projectable,
	ProjectionShape, QueryBuilder, SelectAst, TableProjection, UpdateableTable,
};
use tokio_postgres::Client;

mod query;
mod sql;

#[cfg(feature = "serde")]
pub use query::Json;
pub use query::{
	EmptyRows, PostgresDelete, PostgresDeleteUsing, PostgresInsert, PostgresParam,
	PostgresPreparedMutation, PostgresPreparedSelect, PostgresRowReader, PostgresSelect,
	PostgresSetSelect, PostgresUpdate, PostgresUpdateFrom,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Postgres;

impl Postgres {
	/// This backend's SQL query dialect.
	pub fn dialect(&self) -> impl squealy::Dialect {
		crate::sql::PostgresDialect
	}
}

// Postgres supports `INTERSECT ALL` / `EXCEPT ALL`.
impl squealy::SupportsIntersectExceptAll for Postgres {}

// Postgres can render a columnless upsert: `INSERT INTO t DEFAULT VALUES ON CONFLICT …`.
impl squealy::SupportsColumnlessUpsert for Postgres {}

// Postgres accepts the `DEFAULT` keyword as an assignment value (`VALUES (DEFAULT)`, `SET c = DEFAULT`).
impl squealy::SupportsDefaultKeyword for Postgres {}

// Postgres supports `EXTRACT(<field> FROM <ts>)`.
impl squealy::SupportsExtract for Postgres {}

pub struct PostgresConnection {
	client: Client,
}

impl PostgresConnection {
	pub fn new(client: Client) -> Self {
		Self { client }
	}

	pub(crate) fn client(&self) -> &Client {
		&self.client
	}

	pub(crate) fn client_mut(&mut self) -> &mut Client {
		&mut self.client
	}
}

pub struct PostgresTransaction<'conn> {
	pub(crate) transaction: tokio_postgres::Transaction<'conn>,
}

impl fmt::Debug for PostgresTransaction<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.debug_struct("PostgresTransaction").finish()
	}
}

impl fmt::Debug for PostgresConnection {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.debug_struct("PostgresConnection").finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub enum PostgresError {
	#[error("query returned no rows")]
	NoRows,
	#[error("database error: {0}")]
	Database(#[from] tokio_postgres::Error),
	#[error("decode error: {0}")]
	Decode(#[source] tokio_postgres::Error),
	#[error("could not convert value to {0}")]
	Conversion(&'static str),
	#[error("postgres render error: {0}")]
	Render(#[source] std::io::Error),
}

impl Backend for Postgres {
	type Error = PostgresError;

	type RowReader<'row> = PostgresRowReader<'row>;

	type ParamWriter<'param> = query::PostgresParamWriter<'param>;

	type Param = query::PostgresParam;

	fn param_writer(params: &mut Vec<Self::Param>) -> Self::ParamWriter<'_> {
		query::PostgresParamWriter::new(params)
	}

	fn no_rows_error() -> Self::Error {
		PostgresError::NoRows
	}

	fn render_error(error: std::io::Error) -> Self::Error {
		PostgresError::Render(error)
	}
}

// PostgreSQL renders a `RETURNING` clause, so it can support the `*_returning` query builders.
impl squealy::SupportsReturning for Postgres {}
impl squealy::SupportsFullJoin for Postgres {}

// Postgres supports the query-level named `WINDOW` clause.
impl squealy::SupportsNamedWindow for Postgres {}
impl squealy::SupportsDateTrunc for Postgres {}
impl squealy::Connect for Postgres {
	type Connection = PostgresConnection;
	type Error = PostgresError;

	async fn connect(&self, url: &str) -> Result<PostgresConnection, PostgresError> {
		let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
		// Drive the connection's IO task in the background for the life of the client. If it ends
		// with an error (for example the server dropped the connection) report it through `tracing`
		// rather than discarding it, so the failure isn't only visible as a confusing later query
		// error. The driver error does not contain the connection password.
		tokio::spawn(async move {
			if let Err(error) = connection.await {
				tracing::error!(%error, "postgres connection closed with error");
			}
		});
		Ok(PostgresConnection::new(client))
	}
}
impl QueryBuilder for Postgres {
	type Backend = Postgres;

	type Select<'conn, 'scope, Base, Shape, Projection>
		= PostgresSelect<'conn, 'scope, Shape, Base, Projection, Self>
	where
		Self: 'conn,
		Base: SelectAst<'conn, 'scope, Self> + 'conn,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Projection: Projectable;

	type Insert<'conn, S, Shape, Rows, Returning>
		= PostgresInsert<'conn, S, Shape, Rows, Returning, Self>
	where
		Self: 'conn,
		S: InsertableTable,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Rows: squealy::InsertRows,
		Returning: Projectable;

	type Update<'conn, S, Shape, Columns, Filters, Returning>
		= PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Self>
	where
		Self: 'conn,
		S: UpdateableTable,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Columns: squealy::UpdateAssignments,
		Filters: squealy::PredicateNodes,
		Returning: Projectable;

	type Delete<'conn, S, Shape, Filters, Returning>
		= PostgresDelete<'conn, S, Shape, Filters, Returning, Self>
	where
		Self: 'conn,
		S: TableProjection,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Filters: squealy::PredicateNodes,
		Returning: Projectable;

	type UpdateFrom<'conn, S, O, Columns, Filters>
		= PostgresUpdateFrom<'conn, S, O, Columns, Filters, Self>
	where
		Self: 'conn,
		S: UpdateableTable,
		O: squealy::SchemaTable,
		Columns: squealy::UpdateAssignments,
		Filters: squealy::PredicateNodes;

	type DeleteUsing<'conn, S, O, Filters>
		= PostgresDeleteUsing<'conn, S, O, Filters, Self>
	where
		Self: 'conn,
		S: TableProjection,
		O: TableProjection,
		Filters: squealy::PredicateNodes;
}

impl QueryBuilder for PostgresConnection {
	type Backend = Postgres;

	type Select<'conn, 'scope, Base, Shape, Projection>
		= PostgresSelect<'conn, 'scope, Shape, Base, Projection, Self>
	where
		Self: 'conn,
		Base: SelectAst<'conn, 'scope, Self> + 'conn,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Projection: Projectable;

	type Insert<'conn, S, Shape, Rows, Returning>
		= PostgresInsert<'conn, S, Shape, Rows, Returning, Self>
	where
		Self: 'conn,
		S: InsertableTable,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Rows: squealy::InsertRows,
		Returning: Projectable;

	type Update<'conn, S, Shape, Columns, Filters, Returning>
		= PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Self>
	where
		Self: 'conn,
		S: UpdateableTable,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Columns: squealy::UpdateAssignments,
		Filters: squealy::PredicateNodes,
		Returning: Projectable;

	type Delete<'conn, S, Shape, Filters, Returning>
		= PostgresDelete<'conn, S, Shape, Filters, Returning, Self>
	where
		Self: 'conn,
		S: TableProjection,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Filters: squealy::PredicateNodes,
		Returning: Projectable;

	type UpdateFrom<'conn, S, O, Columns, Filters>
		= PostgresUpdateFrom<'conn, S, O, Columns, Filters, Self>
	where
		Self: 'conn,
		S: UpdateableTable,
		O: squealy::SchemaTable,
		Columns: squealy::UpdateAssignments,
		Filters: squealy::PredicateNodes;

	type DeleteUsing<'conn, S, O, Filters>
		= PostgresDeleteUsing<'conn, S, O, Filters, Self>
	where
		Self: 'conn,
		S: TableProjection,
		O: TableProjection,
		Filters: squealy::PredicateNodes;
}

impl QueryBuilder for PostgresTransaction<'_> {
	type Backend = Postgres;

	type Select<'conn, 'scope, Base, Shape, Projection>
		= PostgresSelect<'conn, 'scope, Shape, Base, Projection, Self>
	where
		Self: 'conn,
		Base: SelectAst<'conn, 'scope, Self> + 'conn,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Projection: Projectable;

	type Insert<'conn, S, Shape, Rows, Returning>
		= PostgresInsert<'conn, S, Shape, Rows, Returning, Self>
	where
		Self: 'conn,
		S: InsertableTable,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Rows: squealy::InsertRows,
		Returning: Projectable;

	type Update<'conn, S, Shape, Columns, Filters, Returning>
		= PostgresUpdate<'conn, S, Shape, Columns, Filters, Returning, Self>
	where
		Self: 'conn,
		S: UpdateableTable,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Columns: squealy::UpdateAssignments,
		Filters: squealy::PredicateNodes,
		Returning: Projectable;

	type Delete<'conn, S, Shape, Filters, Returning>
		= PostgresDelete<'conn, S, Shape, Filters, Returning, Self>
	where
		Self: 'conn,
		S: TableProjection,
		Shape: ProjectionShape,
		Shape::Row: Decode<Self::Backend>,
		Filters: squealy::PredicateNodes,
		Returning: Projectable;

	type UpdateFrom<'conn, S, O, Columns, Filters>
		= PostgresUpdateFrom<'conn, S, O, Columns, Filters, Self>
	where
		Self: 'conn,
		S: UpdateableTable,
		O: squealy::SchemaTable,
		Columns: squealy::UpdateAssignments,
		Filters: squealy::PredicateNodes;

	type DeleteUsing<'conn, S, O, Filters>
		= PostgresDeleteUsing<'conn, S, O, Filters, Self>
	where
		Self: 'conn,
		S: TableProjection,
		O: TableProjection,
		Filters: squealy::PredicateNodes;
}

// Upsert (`INSERT … ON CONFLICT`) is PostgreSQL-only; the conflict clause is a runtime field on the
// existing `PostgresInsert` query object, so `build_upsert` just constructs it with the clause.
macro_rules! impl_on_conflict_query_builder {
	($ty:ty) => {
		impl squealy::OnConflictQueryBuilder for $ty {
			fn build_upsert<'conn, S, Shape, Rows, Returning>(
				&'conn self,
				rows: Rows,
				returning: Returning,
				conflict: squealy::ConflictClause,
			) -> Self::Insert<'conn, S, Shape, Rows, Returning>
			where
				Self: 'conn,
				S: InsertableTable,
				Shape: ProjectionShape,
				Shape::Row: squealy::Decode<Self::Backend>,
				Rows: squealy::InsertRows,
				Returning: Projectable,
			{
				crate::query::PostgresInsert::new_upsert(self, rows, returning, conflict)
			}
		}
	};
}
impl_on_conflict_query_builder!(Postgres);
impl_on_conflict_query_builder!(PostgresConnection);
impl_on_conflict_query_builder!(PostgresTransaction<'_>);

// PostgreSQL renders `FOR UPDATE` / `FOR SHARE`, so `for_update()` / `for_share()` are available.
impl squealy::RendersRowLock for Postgres {}

impl Connection for PostgresConnection {}

impl Connection for PostgresTransaction<'_> {}

impl ConnectionWithTransaction for PostgresConnection {
	type Transaction<'conn>
		= PostgresTransaction<'conn>
	where
		Self: 'conn;

	async fn transaction<'conn, T, F>(
		&'conn mut self,
		f: F,
	) -> Result<T, <Self::Backend as Backend>::Error>
	where
		T: 'conn,
		F: for<'tx> AsyncFnOnce(
				&'tx mut Self::Transaction<'conn>,
			) -> Result<T, <Self::Backend as Backend>::Error>
			+ 'conn,
	{
		let transaction = self
			.client_mut()
			.transaction()
			.await
			.map_err(PostgresError::Database)?;
		let mut transaction: Self::Transaction<'conn> = PostgresTransaction { transaction };

		match f(&mut transaction).await {
			Ok(value) => {
				transaction
					.transaction
					.commit()
					.await
					.map_err(PostgresError::Database)?;
				Ok(value)
			}
			Err(error) => {
				transaction
					.transaction
					.rollback()
					.await
					.map_err(PostgresError::Database)?;
				Err(error)
			}
		}
	}

	async fn transaction_scoped<'conn, T, F>(
		&'conn mut self,
		f: F,
	) -> Result<T, <Self::Backend as Backend>::Error>
	where
		T: 'conn,
		F: for<'tx> FnOnce(
				&'tx mut Self::Transaction<'conn>,
			) -> std::pin::Pin<
				Box<
					dyn std::future::Future<Output = Result<T, <Self::Backend as Backend>::Error>>
						+ Send
						+ 'tx,
				>,
			> + 'conn,
	{
		let transaction = self
			.client_mut()
			.transaction()
			.await
			.map_err(PostgresError::Database)?;
		let mut transaction: Self::Transaction<'conn> = PostgresTransaction { transaction };

		match f(&mut transaction).await {
			Ok(value) => {
				transaction
					.transaction
					.commit()
					.await
					.map_err(PostgresError::Database)?;
				Ok(value)
			}
			Err(error) => {
				transaction
					.transaction
					.rollback()
					.await
					.map_err(PostgresError::Database)?;
				Err(error)
			}
		}
	}
}
