#![forbid(unsafe_code)]

use std::fmt;

use squealy::{
    Backend, Connection, ConnectionWithTransaction, Decode, InsertableTable, Projectable,
    ProjectionShape, QueryBuilder, SelectAst, Table, TableProjection, UpdateableTable,
};
use tokio_postgres::Client;

#[cfg(feature = "schema")]
mod introspect;
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
    /// This backend's [`Dialect`](squealy::Dialect) — the render-side counterpart to the reverse
    /// parser's reader ([`squealy_parse`](https://docs.rs/squealy-parse)). Exposed so a round-trip can
    /// render a scalar expression (a `CHECK` / index / generated-column term) the same way DDL does,
    /// then read it back, and assert the two stay symmetric.
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

/// Canonicalizes a logical [`SqlType`](squealy::SqlType) to the form PostgreSQL introspection reports:
/// `Text` → `String` (both render `text`), a bare (`None`) `Timestamp`/`Time` precision → microseconds
/// (`Some(6)`, PostgreSQL's stored default), and each narrow/unsigned/128-bit integer width → the signed
/// type its rendered PostgreSQL type introspects back to.
///
/// PostgreSQL has only `smallint`/`integer`/`bigint`/`numeric` for integers, so the neutral model's
/// twelve integer widths render to — and read back from — one of those four: `smallint` (`I8`/`I16`/`U8`)
/// → `I16`, `integer` (`I32`/`U16`) → `I32`, `bigint` (`I64`/`Isize`/`U32`/`Usize`) → `I64`, and bare
/// `numeric` (`I128`/`U64`/`U128`) → `I128`. Folding each desired width to that representative on both
/// sides of the diff lets a published column re-plan to empty. The stored column is genuinely that width;
/// the neutral type is lossy on PostgreSQL (a `U8` column is planned as `smallint`).
///
/// An explicit `#[column(db_type = "numeric")]` reaches the model as `Raw("numeric")` (or `Raw("decimal")`)
/// and renders — like `I128`/`U64`/`U128` — to bare `numeric`, which introspection reads back as `I128`.
/// Fold that spelling (case-insensitively — `parse_db_type` preserves the user's casing in the `Raw`
/// fallback) to the same `I128` representative so an explicit arbitrary-precision numeric column also
/// re-plans to empty rather than churning against the introspected `I128`. Any other `Raw` (a typmod'd
/// `numeric(p,s)` is a distinct `Decimal`, not `Raw`) is left untouched.
///
/// The trait [`canonical_sql_type`](squealy::SchemaIntrospect::canonical_sql_type) delegates here so the
/// pure mapping is reusable by [`canonical_pg_pin_type`] and unit-testable without a live connection.
#[cfg(feature = "schema")]
pub(crate) fn canonical_pg_sql_type(ty: &squealy::SqlType) -> squealy::SqlType {
    use squealy::SqlType::{
        I8, I16, I32, I64, I128, Isize, Raw, String as SqlString, Text, Time, Timestamp, U8, U16,
        U32, U64, U128, Usize,
    };
    match ty {
        Text => SqlString,
        Timestamp {
            tz,
            precision: None,
        } => Timestamp {
            tz: *tz,
            precision: Some(6),
        },
        Time {
            tz,
            precision: None,
        } => Time {
            tz: *tz,
            precision: Some(6),
        },
        I8 | U8 => I16,
        U16 => I32,
        Isize | U32 | Usize => I64,
        I128 | U64 | U128 => I128,
        Raw(name)
            if name.eq_ignore_ascii_case("numeric") || name.eq_ignore_ascii_case("decimal") =>
        {
            I128
        }
        other => other.clone(),
    }
}

/// Folds a result-pin [`SqlType`](squealy::SqlType) to the canonical representative PostgreSQL
/// introspection yields for it. The many-to-one integer cast keywords (`smallint`/`integer`/`bigint`/bare
/// `numeric`) and the text/temporal aliases fold through [`canonical_pg_sql_type`]. The one collapse a pin
/// needs beyond a column's is `FixedBytes(N)` → `Bytes`: a view output produced through a pinned
/// expression (a `CASE`/`COALESCE`/aggregate cast) carries no length `CHECK`, so it renders as bare
/// `bytea` and reads back as `Bytes` — mirroring `canonical_view_column_type`. Any other type is unchanged
/// (PostgreSQL's remaining cast spellings are one-to-one).
#[cfg(feature = "schema")]
pub(crate) fn canonical_pg_pin_type(ty: &squealy::SqlType) -> squealy::SqlType {
    use squealy::SqlType::{Bytes, FixedBytes};
    match canonical_pg_sql_type(ty) {
        FixedBytes(_) => Bytes,
        other => other,
    }
}

/// Canonicalizes a column default to the form PostgreSQL introspection reports for it. Every PostgreSQL
/// integer column reads back as a signed neutral type (`smallint` → `I16`, `integer` → `I32`, `bigint` →
/// `I64`, bare `numeric` → `I128`) whose default parses as a signed [`Int`](squealy::DefaultValue::Int).
/// So once [`canonical_pg_sql_type`] has folded a desired unsigned column's type to its signed
/// representative, fold an unsigned literal default the same way, otherwise the column re-plans to churn on
/// the default alone. `ty` is the already-canonicalized column type (see
/// [`canonicalize_model`](squealy::canonicalize_model)), so an integer column is one of
/// `I16`/`I32`/`I64`/`I128`. A value above `i128::MAX` (only reachable on a `U128` default) has no `Int`
/// (`i128`) representation; introspection reads it back as `Raw`, an accepted residual.
#[cfg(feature = "schema")]
pub(crate) fn canonical_pg_default(
    ty: &squealy::SqlType,
    default: &squealy::DefaultValue,
) -> squealy::DefaultValue {
    use squealy::SqlType::{I16, I32, I64, I128};
    if matches!(ty, I16 | I32 | I64 | I128)
        && let squealy::DefaultValue::UInt(value) = default
        && let Ok(signed) = i128::try_from(*value)
    {
        return squealy::DefaultValue::Int(signed);
    }
    default.clone()
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

    fn write_table(
        &self,
        table: &(dyn Table + Sync),
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::write_table(table, writer)
    }
}

// PostgreSQL renders a `RETURNING` clause, so it can support the `*_returning` query builders.
impl squealy::SupportsReturning for Postgres {}
impl squealy::SupportsFullJoin for Postgres {}

// Postgres supports the query-level named `WINDOW` clause.
impl squealy::SupportsNamedWindow for Postgres {}
impl squealy::SupportsDateTrunc for Postgres {}

#[cfg(feature = "schema")]
impl squealy::SchemaBackend for Postgres {
    fn capabilities(&self) -> squealy::SchemaCapabilities {
        squealy::SchemaCapabilities {
            columns: squealy::ColumnCapabilities { on_update: false },
            constraints: squealy::ConstraintCapabilities {
                foreign_key_match_type: true,
                foreign_key_deferrability: true,
                foreign_key_validation: true,
                foreign_key_enforcement: false,
                check_validation: true,
                check_enforcement: false,
            },
            indexes: squealy::IndexCapabilities {
                predicates: true,
                expressions: true,
                include_columns: true,
                null_ordering: true,
                collations: true,
                operator_classes: true,
                prefix_lengths: false,
            },
        }
    }

    fn render_create(
        &self,
        model: &squealy::DatabaseModel,
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::ddl::write_database(model, writer)
    }

    fn render_plan(
        &self,
        plan: &squealy::DatabasePlan,
        // PostgreSQL renders each step's delta in place (`ALTER TABLE … ALTER COLUMN …`), so it does
        // not need the full target model that table-rebuild backends (SQLite) rely on.
        _desired: &squealy::DatabaseModel,
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::ddl::write_plan(plan, writer)
    }

    fn render_plan_concurrent(
        &self,
        plan: &squealy::DatabasePlan,
        _desired: &squealy::DatabaseModel,
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        sql::ddl::write_plan_concurrent(plan, writer)
    }

    fn supports_concurrent_index_creation(&self) -> bool {
        // PostgreSQL builds indexes without locking writes via `CREATE INDEX CONCURRENTLY`.
        true
    }
}

#[cfg(feature = "schema")]
impl squealy::DdlExecutor for PostgresConnection {
    type Error = PostgresError;

    /// Runs the DDL batch inside a transaction so create-from-scratch is all-or-nothing
    /// (PostgreSQL supports transactional DDL).
    async fn execute_ddl(&mut self, sql: &str) -> Result<(), PostgresError> {
        let transaction = self.client_mut().transaction().await?;
        transaction.batch_execute(sql).await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Runs each statement on its own, without a transaction, so statements that cannot run inside a
    /// transaction block (`CREATE INDEX CONCURRENTLY`) work. A failure here leaves earlier statements
    /// applied — that is inherent to non-transactional concurrent index builds.
    async fn execute_ddl_unmanaged(&mut self, sql: &str) -> Result<(), PostgresError> {
        for statement in split_ddl_statements(sql) {
            self.client().batch_execute(statement).await?;
        }
        Ok(())
    }
}

/// Splits a `;`-separated DDL batch into individual statements (trimming the trailing `;`), so each
/// can be executed on its own outside a transaction.
#[cfg(feature = "schema")]
fn split_ddl_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            // A doubled quote (`''`) is an escaped quote inside a string literal; toggling twice
            // nets out, so a simple toggle correctly tracks both quotes and the escape.
            b'\'' => in_string = !in_string,
            b';' if !in_string && bytes.get(index + 1) == Some(&b'\n') => {
                push_ddl_statement(&mut statements, &sql[start..index]);
                index += 1; // skip the '\n'
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    push_ddl_statement(&mut statements, &sql[start..]);
    statements
}

#[cfg(feature = "schema")]
fn push_ddl_statement<'sql>(statements: &mut Vec<&'sql str>, statement: &'sql str) {
    let statement = statement.trim().trim_end_matches(';').trim();
    if !statement.is_empty() {
        statements.push(statement);
    }
}

#[cfg(feature = "schema")]
impl squealy::SchemaConnect for Postgres {
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

#[cfg(feature = "schema")]
impl squealy::SchemaIntrospect for PostgresConnection {
    type Error = PostgresError;

    async fn introspect_database(&mut self) -> Result<squealy::DatabaseModel, PostgresError> {
        introspect::database(self.client()).await
    }

    /// PostgreSQL renders both `String` and `Text` as `text`, which introspects back as `String`;
    /// map `Text` to `String` so a desired model using `Text` does not churn against the live schema.
    ///
    /// A `Time`/`Timestamp` with no explicit precision (`None`) renders as the bare form, which
    /// PostgreSQL stores at its microsecond default and introspection reads back as `Some(6)`;
    /// canonicalize `None` to `Some(6)` so the two sides compare equal.
    fn canonical_sql_type(&self, ty: &squealy::SqlType) -> squealy::SqlType {
        canonical_pg_sql_type(ty)
    }

    /// Every PostgreSQL integer column introspects back as a signed neutral type (`smallint` → `I16`,
    /// `integer` → `I32`, `bigint` → `I64`, bare `numeric` → `I128`) whose default reads as a signed
    /// [`Int`](squealy::DefaultValue::Int). So once [`canonical_sql_type`](Self::canonical_sql_type) has
    /// folded a desired unsigned column's type to its signed representative, fold an unsigned literal
    /// default the same way, otherwise the column re-plans to churn on the default alone. `ty` is already
    /// canonicalized (see [`canonicalize_model`](squealy::canonicalize_model)), so an integer column is
    /// one of `I16`/`I32`/`I64`/`I128`. A value above `i128::MAX` (only reachable on a `U128` default) has
    /// no `Int` (`i128`) representation; introspection reads it back as `Raw`, an accepted residual.
    fn canonical_default(
        &self,
        ty: &squealy::SqlType,
        default: &squealy::DefaultValue,
    ) -> squealy::DefaultValue {
        canonical_pg_default(ty, default)
    }

    /// A PostgreSQL view column declared `FixedBytes(N)` introspects as plain `bytea` (`Bytes`) — there
    /// is no generated length `CHECK` on a view column to fold back into `FixedBytes`. Canonicalize it
    /// to `Bytes` so a view with a fixed-byte output column does not churn a drop+recreate every run.
    fn canonical_view_column_type(&self, ty: &squealy::SqlType) -> squealy::SqlType {
        match self.canonical_sql_type(ty) {
            squealy::SqlType::FixedBytes(_) => squealy::SqlType::Bytes,
            other => other,
        }
    }

    /// Folds a reconstructed view body's result-pin types to the canonical representative PostgreSQL
    /// introspection yields for each, so a published view whose body the reverse parser now reconstructs
    /// re-plans to empty. Applied to both the desired and the introspected model (see
    /// [`SchemaIntrospect::canonical_view_body`]).
    fn canonical_view_body(&self, mut body: squealy::ViewBody) -> squealy::ViewBody {
        body.map_result_pins(&canonical_pg_pin_type);
        // A general cast in a view body (now structured by the reverse parser) folds its target the same
        // way a table check does — a many-to-one dialect spelling round-trips as the representative, so a
        // structural desired cast does not churn.
        body.map_exprs(&|node| {
            if let squealy::ExprNode::Cast { ty, .. } = node {
                *ty = canonical_pg_pin_type(ty);
            }
        });
        body
    }

    fn canonical_cast_type(&self, ty: &squealy::SqlType) -> squealy::SqlType {
        canonical_pg_pin_type(ty)
    }

    /// PostgreSQL introspection reports a plain index's access method as `btree`; map an unset
    /// method to that so a crate-declared index does not churn against the live schema.
    fn default_index_method(&self) -> Option<squealy::IndexMethod> {
        Some(squealy::IndexMethod::BTree)
    }

    /// Structures a `Raw` partial-index predicate (a legacy package's verbatim `WHERE`, or an
    /// un-invertible introspected one) by re-parsing it in the PostgreSQL dialect, so it compares equal to
    /// a freshly introspected structural predicate. An already-structural predicate is returned unchanged.
    ///
    /// An un-structurable `Raw` is kept **verbatim** (not string-normalized): the canonical model feeds the
    /// rendered `CREATE INDEX … WHERE`, so rewriting a raw predicate here — e.g. stripping a cast that a
    /// user-defined overloaded function's resolution depends on — could build a different partial index.
    fn canonical_index_predicate(&self, predicate: squealy::ExprNode) -> squealy::ExprNode {
        match predicate {
            squealy::ExprNode::Raw(sql) => {
                squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
                    .read_index_predicate_or_raw(&sql)
            }
            other => other,
        }
    }

    /// Structures a `Raw` check expression (a legacy package's verbatim check, or an un-invertible
    /// introspected one) by re-parsing it in the PostgreSQL dialect, so it compares equal to a freshly
    /// introspected structural check. An already-structural expression is returned unchanged. This is the
    /// same shape as the MySQL/SQLite seams — a plain re-parse, no string normalizer.
    ///
    /// The reverse parser lowers PostgreSQL's `pg_get_constraintdef` deparse idioms structurally —
    /// general function calls, `%` modulo, `= ANY(ARRAY[..])`/`<> ALL`, `~~`/`~~*` (LIKE/ILIKE), the
    /// `BETWEEN` expansion, synthesized redundant literal casts, and a general `CAST` — so both the
    /// desired (Raw-then-re-parsed) and introspected sides structure identically, and equivalent checks
    /// compare equal without the former `canonical.rs` string normalizer. A residual shape the grammar
    /// still cannot invert — `IS TRUE`/`IS FALSE` (no neutral node), a cast to a non-modeled type
    /// (`::inet`, an enum), a quoted function name, a general function with a direct literal argument
    /// (`my_func('x')`, which `pg_get_constraintdef` deparses as `my_func('x'::text)` — the synthesized
    /// argument cast cannot be stripped without risking a different overload), or PostgreSQL's
    /// synthesized converting literal cast on a bare literal (e.g. `(1.5)::double precision` vs an
    /// authored `1.5`) — stays `Raw` and is compared verbatim (a documented churn, never corruption).
    fn canonical_check_expression(&self, expression: squealy::ExprNode) -> squealy::ExprNode {
        match expression {
            squealy::ExprNode::Raw(sql) => {
                squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
                    .read_check_expression_or_raw(&sql)
            }
            other => other,
        }
    }

    /// Structures a `Raw` generated-column expression (a legacy package's verbatim one, or an
    /// un-invertible introspected `pg_get_expr` deparse) by re-parsing it in the PostgreSQL dialect, so it
    /// compares equal to a freshly introspected structural one. An already-structural expression is
    /// returned unchanged.
    ///
    /// A term that stays outside the structural grammar is kept **verbatim** as `Raw` — like a
    /// [`canonical_index_expression`](Self::canonical_index_expression) term (and unlike a `CHECK`), it is
    /// NOT run through the string canonicalizer, since the canonical model feeds the rendered
    /// `GENERATED ALWAYS AS (…)` and rewriting the raw text could change the computed column.
    fn canonical_generated_expression(&self, expression: squealy::ExprNode) -> squealy::ExprNode {
        match expression {
            squealy::ExprNode::Raw(sql) => {
                squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
                    .read_generated_expression_or_raw(&sql)
            }
            other => other,
        }
    }

    /// Structures a `Raw` index-key expression (a legacy package's verbatim term, or an un-invertible
    /// introspected one) by re-parsing it in the PostgreSQL dialect, so it compares equal to a freshly
    /// introspected structural expression index. An already-structural expression is returned unchanged.
    ///
    /// A single legacy `Raw` may carry a whole comma-separated key (`lower(a), upper(b)`, the old
    /// introspector's one-term form), so this re-splits via `read_index_expressions_or_raw` into the
    /// per-term structural vector live introspection now produces. A term that stays outside the structural
    /// grammar is kept **verbatim** as `Raw` — unlike a `CHECK` expression it is NOT run through the string
    /// canonicalizer, which strips literal casts (`my_func('x'::text)` → `my_func('x')`) and so could change
    /// overload resolution for a user-defined function; the canonical model feeds the rendered
    /// `CREATE INDEX`, so rewriting a raw term here would build a different index.
    fn canonical_index_expression(&self, expression: squealy::ExprNode) -> Vec<squealy::ExprNode> {
        match expression {
            squealy::ExprNode::Raw(sql) => {
                squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
                    .read_index_expressions_or_raw(&sql)
            }
            other => vec![other],
        }
    }
}

#[cfg(feature = "schema")]
impl squealy::SchemaRefactorStore for PostgresConnection {
    type Error = PostgresError;

    async fn applied_refactor_ids(&mut self) -> Result<Vec<String>, PostgresError> {
        let exists = self
            .client()
            .query_one(
                "SELECT to_regclass('\"__squealy\".\"refactors\"') IS NOT NULL",
                &[],
            )
            .await?
            .get::<_, bool>(0);
        if !exists {
            return Ok(Vec::new());
        }

        Ok(self
            .client()
            .query(
                "SELECT \"id\" FROM \"__squealy\".\"refactors\" ORDER BY \"id\"",
                &[],
            )
            .await?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }

    async fn record_applied_refactor_ids(&mut self, ids: &[String]) -> Result<(), PostgresError> {
        if ids.is_empty() {
            return Ok(());
        }

        self.client()
            .batch_execute(
                "\
CREATE SCHEMA IF NOT EXISTS \"__squealy\";
CREATE TABLE IF NOT EXISTS \"__squealy\".\"refactors\" (
    \"id\" text PRIMARY KEY,
    \"applied_at\" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
)",
            )
            .await?;

        for id in ids {
            self.client()
                .execute(
                    "\
INSERT INTO \"__squealy\".\"refactors\" (\"id\")
VALUES ($1)
ON CONFLICT (\"id\") DO NOTHING",
                    &[id],
                )
                .await?;
        }

        Ok(())
    }
}

#[cfg(feature = "schema")]
impl squealy::SchemaMetadataStore for PostgresConnection {
    type Error = PostgresError;

    async fn schema_metadata(&mut self) -> Result<Vec<(String, String)>, PostgresError> {
        let exists = self
            .client()
            .query_one(
                "SELECT to_regclass('\"__squealy\".\"metadata\"') IS NOT NULL",
                &[],
            )
            .await?
            .get::<_, bool>(0);
        if !exists {
            return Ok(Vec::new());
        }

        Ok(self
            .client()
            .query(
                "SELECT \"name\", \"value\" FROM \"__squealy\".\"metadata\" ORDER BY \"name\"",
                &[],
            )
            .await?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }

    async fn record_schema_metadata(
        &mut self,
        entries: &[(String, String)],
    ) -> Result<(), PostgresError> {
        if entries.is_empty() {
            return Ok(());
        }

        self.client()
            .batch_execute(
                "\
CREATE SCHEMA IF NOT EXISTS \"__squealy\";
CREATE TABLE IF NOT EXISTS \"__squealy\".\"metadata\" (
    \"name\" text PRIMARY KEY,
    \"value\" text NOT NULL,
    \"updated_at\" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
)",
            )
            .await?;

        for (name, value) in entries {
            self.client()
                .execute(
                    "\
INSERT INTO \"__squealy\".\"metadata\" (\"name\", \"value\")
VALUES ($1, $2)
ON CONFLICT (\"name\") DO UPDATE
SET \"value\" = EXCLUDED.\"value\", \"updated_at\" = CURRENT_TIMESTAMP",
                    &[name, value],
                )
                .await?;
        }

        Ok(())
    }
}

#[cfg(feature = "schema")]
impl squealy::SchemaPublishHistoryStore for PostgresConnection {
    type Error = PostgresError;

    async fn schema_publish_history(
        &mut self,
        limit: usize,
    ) -> Result<Vec<squealy::SchemaPublishRecord>, PostgresError> {
        let exists = self
            .client()
            .query_one(
                "SELECT to_regclass('\"__squealy\".\"publish_history\"') IS NOT NULL",
                &[],
            )
            .await?
            .get::<_, bool>(0);
        if !exists || limit == 0 {
            return Ok(Vec::new());
        }

        Ok(self
            .client()
            .query(
                "\
SELECT \"mode\",
       \"package_hash\",
       \"package_format_version\",
       to_char(\"applied_at\" AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
FROM \"__squealy\".\"publish_history\"
ORDER BY \"id\" DESC
LIMIT $1",
                &[&(limit as i64)],
            )
            .await?
            .into_iter()
            .map(|row| squealy::SchemaPublishRecord {
                mode: row.get(0),
                package_hash: row.get(1),
                package_format_version: row.get(2),
                applied_at: row.get(3),
            })
            .collect())
    }

    async fn record_schema_publish(
        &mut self,
        mode: &str,
        package_hash: &str,
        package_format_version: &str,
    ) -> Result<(), PostgresError> {
        self.client()
            .batch_execute(
                "\
CREATE SCHEMA IF NOT EXISTS \"__squealy\";
CREATE TABLE IF NOT EXISTS \"__squealy\".\"publish_history\" (
    \"id\" bigserial PRIMARY KEY,
    \"mode\" text NOT NULL,
    \"package_hash\" text NOT NULL,
    \"package_format_version\" text NOT NULL,
    \"applied_at\" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
)",
            )
            .await?;

        self.client()
            .execute(
                "\
INSERT INTO \"__squealy\".\"publish_history\" (
    \"mode\",
    \"package_hash\",
    \"package_format_version\"
)
VALUES ($1, $2, $3)",
                &[&mode, &package_hash, &package_format_version],
            )
            .await?;

        Ok(())
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

#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::split_ddl_statements;

    #[test]
    fn split_ddl_statements_respects_string_literals() {
        // A `;\n` inside a single-quoted literal (e.g. an index predicate) must not split the batch.
        let sql = "CREATE INDEX CONCURRENTLY i ON t (c) WHERE note = 'a;\nb';\n\
CREATE INDEX CONCURRENTLY j ON t (d);";
        let statements = split_ddl_statements(sql);
        assert_eq!(
            statements,
            vec![
                "CREATE INDEX CONCURRENTLY i ON t (c) WHERE note = 'a;\nb'",
                "CREATE INDEX CONCURRENTLY j ON t (d)",
            ]
        );
    }

    #[test]
    fn split_ddl_statements_handles_escaped_quotes() {
        let statements = split_ddl_statements("SET x = 'a''b';\nSET y = 1;");
        assert_eq!(statements, vec!["SET x = 'a''b'", "SET y = 1"]);
    }

    #[test]
    fn canonical_pg_pin_type_collapses_many_to_one_integer_casts() {
        use super::canonical_pg_pin_type;
        use squealy::SqlType::{
            Bytes, Decimal, F64, I8, I16, I32, I64, I128, Isize, String as SqlString, Text,
            Timestamp, U8, U16, U32, U64, U128, Usize,
        };

        // Each PostgreSQL cast keyword is many-to-one; the reverse parser inverts it to one
        // representative, so the desired side's narrower pin must fold to the same one.
        for ty in [I8, I16, U8] {
            assert_eq!(canonical_pg_pin_type(&ty), I16); // smallint
        }
        for ty in [I32, U16] {
            assert_eq!(canonical_pg_pin_type(&ty), I32); // integer
        }
        for ty in [I64, Isize, U32, Usize] {
            assert_eq!(canonical_pg_pin_type(&ty), I64); // bigint
        }
        for ty in [I128, U64, U128] {
            assert_eq!(canonical_pg_pin_type(&ty), I128); // bare numeric
        }
        // Text/temporal aliases fold through `canonical_pg_sql_type`.
        assert_eq!(canonical_pg_pin_type(&Text), SqlString);
        assert_eq!(
            canonical_pg_pin_type(&Timestamp {
                tz: false,
                precision: None,
            }),
            Timestamp {
                tz: false,
                precision: Some(6),
            }
        );
        // A `FixedBytes(N)` pin renders `bytea` and reads back as `Bytes` (no length CHECK on a view
        // output), mirroring `canonical_view_column_type`.
        assert_eq!(
            canonical_pg_pin_type(&squealy::SqlType::FixedBytes(16)),
            Bytes
        );
        // One-to-one spellings are unchanged.
        assert_eq!(canonical_pg_pin_type(&F64), F64);
        assert_eq!(
            canonical_pg_pin_type(&Decimal {
                precision: 10,
                scale: 2,
            }),
            Decimal {
                precision: 10,
                scale: 2,
            }
        );
        // Idempotent on every representative (so applying it to the already-canonical introspected side
        // is a no-op).
        for ty in [I16, I32, I64, I128, SqlString, F64] {
            let once = canonical_pg_pin_type(&ty);
            assert_eq!(canonical_pg_pin_type(&once), once);
        }
    }

    #[test]
    fn canonical_pg_sql_type_collapses_narrow_and_wide_integer_columns() {
        use super::canonical_pg_sql_type;
        use squealy::SqlType::{I8, I16, I32, I64, I128, Isize, U8, U16, U32, U64, U128, Usize};

        // The neutral model's twelve integer widths render to PostgreSQL's four integer types, so a
        // column must canonicalize to the signed representative its rendered type introspects back to.
        for ty in [I8, I16, U8] {
            assert_eq!(canonical_pg_sql_type(&ty), I16); // smallint
        }
        for ty in [I32, U16] {
            assert_eq!(canonical_pg_sql_type(&ty), I32); // integer
        }
        for ty in [I64, Isize, U32, Usize] {
            assert_eq!(canonical_pg_sql_type(&ty), I64); // bigint
        }
        for ty in [I128, U64, U128] {
            assert_eq!(canonical_pg_sql_type(&ty), I128); // bare numeric
        }
        // An explicit `db_type = "numeric"`/`"decimal"` (`Raw`) renders to bare numeric too, so it folds
        // to the same `I128` representative — an explicit arbitrary-precision numeric column re-plans to
        // empty instead of churning against the introspected `I128`. A different `Raw` is left alone.
        // Case-insensitive — `parse_db_type` preserves the user's casing in the `Raw` fallback.
        for name in ["numeric", "decimal", "NUMERIC", "Decimal"] {
            assert_eq!(
                canonical_pg_sql_type(&squealy::SqlType::Raw(name.to_owned())),
                I128
            );
        }
        assert_eq!(
            canonical_pg_sql_type(&squealy::SqlType::Raw("citext".to_owned())),
            squealy::SqlType::Raw("citext".to_owned())
        );
        // Idempotent on each representative — the introspected side is already canonical.
        for ty in [I16, I32, I64, I128] {
            assert_eq!(canonical_pg_sql_type(&ty), ty);
        }
    }

    #[test]
    #[cfg(feature = "schema")]
    fn canonical_default_folds_unsigned_integer_defaults_to_signed() {
        use super::canonical_pg_default;
        use squealy::DefaultValue::{Int, Text, UInt};
        use squealy::SqlType::{I16, I32, I64, I128, String as SqlString};

        // An unsigned default on an (already-canonicalized) integer column folds to the signed `Int` form
        // introspection produces, so an unsigned column with a default re-plans to empty.
        assert_eq!(canonical_pg_default(&I16, &UInt(200)), Int(200));
        assert_eq!(canonical_pg_default(&I32, &UInt(70_000)), Int(70_000));
        assert_eq!(canonical_pg_default(&I64, &UInt(5)), Int(5));
        assert_eq!(canonical_pg_default(&I128, &UInt(5)), Int(5));
        // A `u64::MAX` default (the widest `U64` value) still fits `i128` and folds.
        assert_eq!(
            canonical_pg_default(&I128, &UInt(u64::MAX as u128)),
            Int(u64::MAX as i128)
        );
        // A value above `i128::MAX` (only reachable on `U128`) has no `Int` form — left as-is (an
        // accepted residual; introspection reads such a default back as `Raw`).
        let huge = UInt(u128::MAX);
        assert_eq!(canonical_pg_default(&I128, &huge), huge);
        // A signed default and a non-integer column are untouched.
        assert_eq!(canonical_pg_default(&I64, &Int(-5)), Int(-5));
        assert_eq!(
            canonical_pg_default(&SqlString, &Text("x".to_owned())),
            Text("x".to_owned())
        );
    }

    #[test]
    fn a_published_view_with_a_narrow_pin_replans_to_empty_after_canonicalization() {
        use super::{Postgres, canonical_pg_pin_type};
        use squealy::{
            AggregateFunc, ExprNode, ProjectionItem, SourceItem, SourceRef, SqlType, ViewBody,
            ViewColumnModel, ViewModel, ViewQueryModel,
        };
        use squealy_parse::{Reader, SqlDialect};

        // `CREATE VIEW totals (s) AS SELECT CAST(sum(q.x) AS integer) AS s FROM public.t q` — the output
        // column is `U16`, whose result pin PostgreSQL spells as `integer` (shared with `I32`).
        let view = ViewModel {
            name: "totals".to_owned(),
            comment: None,
            columns: vec![ViewColumnModel {
                name: "s".to_owned(),
                ty: SqlType::U16,
                nullable: true,
            }],
            query: ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "s".to_owned(),
                    internal_alias: None,
                    expr: ExprNode::Aggregate {
                        func: AggregateFunc::Sum,
                        distinct: false,
                        operand: Box::new(ExprNode::Column {
                            alias: "q".to_owned(),
                            column: "x".to_owned(),
                        }),
                        result: Some(SqlType::U16),
                    },
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: Some("public".to_owned()),
                    name: "t".to_owned(),
                    alias: "q".to_owned(),
                })),
                ..Default::default()
            })),
        };

        let mut sql = Vec::new();
        squealy::render_create_view(Some("public"), &view, false, &Postgres.dialect(), &mut sql)
            .expect("view renders");
        let sql = std::string::String::from_utf8(sql).expect("SQL is UTF-8");
        // The offline analog of introspection: the reverse parser reads the rendered definition back.
        let lowered = Reader::new(SqlDialect::Postgres)
            .read_create_view(&sql)
            .expect("view body lowers");

        // The narrow `U16` pin round-trips as the canonical `I32` (both spell `integer`), so the raw
        // bodies differ — the churn this seam removes...
        assert_ne!(view.query, lowered);
        // ...and folding both sides through `canonical_pg_pin_type` (what `canonical_view_body` applies)
        // makes them structurally equal, so the published view re-plans to empty.
        let mut desired = view.query.clone();
        desired.map_result_pins(&canonical_pg_pin_type);
        let mut actual = lowered;
        actual.map_result_pins(&canonical_pg_pin_type);
        assert_eq!(desired, actual);
    }

    #[test]
    fn canonical_view_body_folds_a_general_cast_in_the_body() {
        use super::canonical_pg_pin_type;
        use squealy::{
            ExprNode, ProjectionItem, SourceItem, SourceRef, SqlType, ViewBody, ViewQueryModel,
        };
        // A general cast in a view-body projection folds its target the same way a table check's does:
        // a narrow `I8` and the introspected `I16` both spell `smallint`, so folding both sides through
        // the representative makes an otherwise-identical structural view re-plan to empty.
        let cast_view = |ty: SqlType| {
            ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "c".to_owned(),
                    internal_alias: None,
                    expr: ExprNode::Cast {
                        operand: Box::new(ExprNode::Column {
                            alias: "q".to_owned(),
                            column: "x".to_owned(),
                        }),
                        ty,
                    },
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: Some("public".to_owned()),
                    name: "t".to_owned(),
                    alias: "q".to_owned(),
                })),
                ..Default::default()
            }))
        };
        let mut desired = cast_view(SqlType::I8);
        let mut actual = cast_view(SqlType::I16);
        assert_ne!(desired, actual);
        // The general-cast fold `canonical_view_body` applies (via `map_exprs`).
        let fold = |body: &mut ViewBody| {
            body.map_exprs(&|node| {
                if let ExprNode::Cast { ty, .. } = node {
                    *ty = canonical_pg_pin_type(ty);
                }
            });
        };
        fold(&mut desired);
        fold(&mut actual);
        assert_eq!(desired, actual);
    }
}
