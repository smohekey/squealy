# Squealy

Squealy is a typed, asynchronous SQL query builder for Rust. Rust types define table, schema, view,
and CTE metadata; the query API uses those types to check projections, joins, predicates, aggregates,
mutations, backend capabilities, and row decoding at compile time.

PostgreSQL, MySQL, SQLite, and an in-memory SQL-rendering test backend are supported. The minimum
supported Rust version is 1.92.

## Define metadata and query it

The `squealy` crate is the public facade. It re-exports the backend-neutral API and the `Table`,
`Schema`, `Database`, `ColumnType`, `View`, `CTE`, and `RecursiveCTE` derives.

```rust,no_run
use squealy::*;
use squealy_sqlite::{Sqlite, SqliteError};

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i64>,
    email: C::Type<'scope, String>,
    display_name: C::Type<'scope, Option<String>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct App {
    users: User<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct ApplicationDatabase {
    app: App,
}

async fn active_users() -> Result<Vec<(i64, String)>, SqliteError> {
    let connection = Sqlite.connect("sqlite://application.db").await?;

    connection
        .from::<User>()
        .where_(|user| user.email.like("%@example.com"))
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| (user.id, user.email))
        .collect()
        .await
}
```

`DatabaseModel::from_database::<ApplicationDatabase>()` exports the derived tables, constraints,
indexes, and views as owned metadata. The owned model also retains public shapes for enums, domains,
sequences, exclusions, and materialized views; derives leave collections they cannot author empty.

The query surface includes typed selects and joins, projections, predicates, aggregates, grouping,
windows, subqueries, views, CTEs, set operations, ordering, row locks, inserts, insert-select,
upserts, updates, update-from, deletes, delete-using, returning rows, runtime parameters, prepared
queries, streams, and transactions. Backend-specific capabilities are enforced through the type
system.

## Backends

Add `squealy` plus the backend crate you use:

- `squealy-postgresql` uses `tokio-postgres`.
- `squealy-mysql` uses `mysql_async` with Rustls.
- `squealy-sqlite` uses `tokio-rusqlite` and bundled SQLite.
- `squealy-test` renders and records queries without a database server.

Each production backend exports a zero-sized marker (`Postgres`, `Mysql`, or `Sqlite`) and a
connection wrapper. The markers implement the shared connection trait:

```rust
pub trait Connect {
    type Connection: Connection;
    type Error;

    fn connect(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<Self::Connection, Self::Error>> + Send;
}
```

`Postgres::connect` starts the driver's connection task, `Mysql::connect` selects a UTC session time
zone when timestamp support is enabled, and `Sqlite::connect` handles file URIs and enables foreign
keys. `PostgresConnection::new`, `MysqlConnection::new`, and `SqliteConnection::new` wrap existing
driver connections without repeating that setup.

## Feature flags

The facade and backend crates default to no optional value-type features.

| Feature | Rust value support | Available on |
| --- | --- | --- |
| `uuid` | `uuid::Uuid` | facade, PostgreSQL, MySQL |
| `bytes` | `bytes::Bytes` | facade, PostgreSQL, MySQL |
| `systemtime` | `std::time::SystemTime` | facade, PostgreSQL, MySQL, test backend |
| `time` | `time::OffsetDateTime` | facade, PostgreSQL, MySQL |
| `chrono` | `chrono::DateTime<Utc>` | facade, PostgreSQL, MySQL |
| `serde` | backend JSON wrapper for serde values | PostgreSQL, MySQL |

Enable a value type on both `squealy` and the selected backend. A backend feature forwards the
matching facade feature, so enabling it on the backend dependency is sufficient when Cargo unifies
features.

## Project boundary

Squealy describes Rust-authored metadata and executes typed queries. It does not create schemas,
compare database layouts, or plan/apply schema changes. Create tables and other database objects with
your deployment tooling before executing Squealy queries.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo +1.92 check --workspace --all-targets --all-features
```

PostgreSQL and MySQL integration tests are ignored unless their service URLs are provided; the CI
workflow and `compose.yaml` define suitable local services. SQLite integration tests run normally.
