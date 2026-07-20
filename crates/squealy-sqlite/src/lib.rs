//! SQLite backend for squealy.
//!
//! Renders the SQLite dialect (double-quoted identifiers, type affinities
//! (`INTEGER`/`REAL`/`TEXT`/`BLOB`/`NUMERIC`), `INTEGER PRIMARY KEY AUTOINCREMENT` identity, and
//! **inline** foreign keys — SQLite cannot `ALTER TABLE … ADD CONSTRAINT`) for both schema management
//! (DDL rendering against the core [`DatabaseModel`]) and query execution. The query runtime — the
//! value codec, [`Backend`](squealy::Backend), and the executable query objects — lives in [`query`]
//! and runs against a live database through [`tokio-rusqlite`](tokio_rusqlite), which owns the
//! (`!Send`) `rusqlite::Connection` on a dedicated thread and hands it closures over a channel.
//!
//! Introspection ([`SchemaIntrospect`]) reads a live database back into a [`DatabaseModel`] via
//! `sqlite_master` and the PRAGMA table-valued functions (see [`introspect`]), and the backend-owned
//! schema-management bookkeeping ([`SchemaRefactorStore`]/[`SchemaMetadataStore`]/
//! [`SchemaPublishHistoryStore`]) lives in `__squealy_`-prefixed tables (SQLite has no schemas to put
//! them in). Incremental plan rendering ([`SchemaBackend::render_plan`]) applies the changes SQLite's
//! `ALTER TABLE` supports natively (add/drop/rename column, rename table, create/drop index) directly,
//! and every other change — a column type change, or adding/dropping/altering a primary key, unique,
//! foreign key or check, which SQLite carries only inline in `CREATE TABLE` — by rebuilding the whole
//! table (create new, copy, drop, rename). Because dropping a table fires `ON DELETE` actions while
//! foreign keys are enforced, [`DdlExecutor::execute_ddl`] applies a plan with enforcement disabled and
//! re-checks it before committing (SQLite only lets foreign-key enforcement be toggled outside a
//! transaction, so the executor — not the rendered SQL — owns that envelope).

#![forbid(unsafe_code)]

use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;

use squealy::{
    CheckModel, ConnectionWithTransaction, Constraint, DatabaseModel, DatabasePlan, DdlExecutor,
    DefaultValue, ForeignKeyModel, IdentityMode, SchemaBackend, SchemaConnect, SchemaIntrospect,
    SchemaMetadataStore, SchemaPublishHistoryStore, SchemaPublishRecord, SchemaRefactorStore,
    SqlType,
};
use tokio_rusqlite::rusqlite::{self, OptionalExtension};

mod introspect;
mod query;
mod sql;

pub use query::{SqliteRowReader, SqliteTransaction, SqliteValue};

/// The SQLite schema/query backend marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sqlite;

impl Sqlite {
    /// This backend's [`Dialect`](squealy::Dialect) — the render-side counterpart to the reverse
    /// parser's reader ([`squealy_parse`](https://docs.rs/squealy-parse)). Exposed so a round-trip can
    /// render a scalar expression (a `CHECK` / index / generated-column term) the same way DDL does,
    /// then read it back, and assert the two stay symmetric.
    pub fn dialect(&self) -> impl squealy::Dialect {
        crate::sql::SqliteDialect
    }
}

// SQLite (3.25+) supports window functions and the query-level named `WINDOW` clause.
impl squealy::SupportsNamedWindow for Sqlite {}

/// An error connecting to, executing DDL against, decoding a result column, encoding a bind
/// parameter, or querying SQLite.
#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
    #[error("sqlite connect error: {0}")]
    Connect(#[source] tokio_rusqlite::rusqlite::Error),
    #[error("sqlite ddl error: {0}")]
    Execute(#[source] tokio_rusqlite::Error),
    #[error("sqlite introspection error: {0}")]
    Introspect(#[source] tokio_rusqlite::Error),
    #[error("sqlite query error: {0}")]
    Query(#[source] tokio_rusqlite::Error),
    #[error(
        "refusing to apply: SQLite must drop and recreate a view to apply this plan, which would \
         destroy INSTEAD OF triggers squealy does not model and cannot recreate ({}). Recreate them \
         yourself after applying, and re-run with a policy that allows destructive changes to proceed.",
        triggers.join(", ")
    )]
    DependentTriggers { triggers: Vec<String> },
    #[error("query returned no rows")]
    NoRows,
    #[error("row is missing column {0}")]
    MissingColumn(usize),
    #[error("could not decode a {target} from a SQLite {found} value")]
    Decode {
        target: &'static str,
        found: &'static str,
    },
    #[error("could not convert value to {0}")]
    Conversion(&'static str),
    #[error("sqlite render error: {0}")]
    Render(#[source] std::io::Error),
}

impl SchemaBackend for Sqlite {
    fn capabilities(&self) -> squealy::SchemaCapabilities {
        // Mirrors what the renderer accepts: SQLite supports partial (predicate) indexes, but none of
        // the other index metadata, and no constraint validation/enforcement/deferrability/match
        // metadata. Without advertising `predicates`, the schema engine's `check_create` would reject a
        // partial index before this backend ever rendered it.
        squealy::SchemaCapabilities {
            columns: squealy::ColumnCapabilities::default(),
            constraints: squealy::ConstraintCapabilities::default(),
            indexes: squealy::IndexCapabilities {
                predicates: true,
                ..squealy::IndexCapabilities::default()
            },
            // SQLite has no user-defined enum type.
            enums: false,
            // SQLite has no sequence object (`AUTOINCREMENT` only).
            sequences: false,
            // SQLite has no domain object.
            domains: false,
            // SQLite has no materialized views.
            materialized_views: false,
        }
    }

    fn render_create(&self, model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
        sql::write_database(model, writer)
    }

    fn render_plan(
        &self,
        plan: &DatabasePlan,
        desired: &DatabaseModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        sql::write_plan(plan, desired, writer)
    }
}

/// A live SQLite connection for schema management and query execution.
///
/// Wraps a [`tokio_rusqlite::Connection`], a cheap [`Clone`] handle that owns the underlying
/// (`!Send`) `rusqlite::Connection` on a dedicated thread and runs every closure submitted through
/// [`call`](tokio_rusqlite::Connection::call) FIFO on that thread. Query execution therefore needs
/// only `&self` (no `Mutex`, unlike the MySQL backend): the handle serializes access itself.
pub struct SqliteConnection {
    pub(crate) conn: tokio_rusqlite::Connection,
}

impl SqliteConnection {
    /// Wraps an already-established [`tokio_rusqlite::Connection`].
    ///
    /// This does not configure the connection. [`Sqlite::connect`](SchemaConnect::connect) enables
    /// foreign-key enforcement (`PRAGMA foreign_keys = ON`), which this backend's **inline** foreign
    /// keys rely on; a connection built directly here should run that pragma before relying on it.
    pub fn new(conn: tokio_rusqlite::Connection) -> Self {
        Self { conn }
    }
}

impl std::fmt::Debug for SqliteConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SqliteConnection").finish()
    }
}

impl SchemaConnect for Sqlite {
    type Connection = SqliteConnection;
    type Error = SqliteError;

    /// Opens a SQLite database. `url` may be:
    /// - a native SQLite `file:` URI (e.g. `file:app.db?mode=ro`, `file::memory:?cache=shared`), passed
    ///   through verbatim so SQLite parses its parameters (rusqlite enables URI parsing by default);
    /// - `":memory:"` (or an empty string, or `sqlite://`) for a private in-memory database;
    /// - otherwise a filesystem path, with an optional `sqlite://` convenience prefix stripped.
    ///
    /// Foreign-key enforcement is enabled on the fresh connection (`PRAGMA foreign_keys = ON`) — it is
    /// off by default in SQLite and must be set outside any transaction — so the inline foreign keys
    /// this backend renders are enforced at runtime.
    async fn connect(&self, url: &str) -> Result<SqliteConnection, SqliteError> {
        let conn = if url.starts_with("file:") {
            // A native SQLite URI: open with URI parsing explicitly enabled so its `?`-parameters are
            // honored. (`open` already enables it via `OpenFlags::default`, but spelling out the flag
            // documents the intent and is robust to a future default change.)
            tokio_rusqlite::Connection::open_with_flags(
                url,
                tokio_rusqlite::OpenFlags::default() | tokio_rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .await
        } else {
            let path = url.strip_prefix("sqlite://").unwrap_or(url);
            if path.is_empty() || path == ":memory:" {
                tokio_rusqlite::Connection::open_in_memory().await
            } else {
                tokio_rusqlite::Connection::open(path).await
            }
        }
        .map_err(SqliteError::Connect)?;
        // Foreign keys are off by default and the setting is a no-op inside a transaction, so enable it
        // now on the idle connection. `execute_batch` (not `execute`) because a `PRAGMA` returns no rows.
        conn.call(|conn| conn.execute_batch("PRAGMA foreign_keys = ON"))
            .await
            .map_err(SqliteError::Execute)?;
        Ok(SqliteConnection::new(conn))
    }
}

impl DdlExecutor for SqliteConnection {
    type Error = SqliteError;

    /// Runs the whole DDL batch **atomically** inside a transaction. Unlike MySQL, SQLite has
    /// transactional DDL, so a mid-batch failure rolls the whole batch back — there is no
    /// partially-applied-schema state to report.
    ///
    /// Foreign-key enforcement is disabled for the batch and restored to its previous setting
    /// afterwards. An incremental plan rebuilds a changed table by dropping and recreating it, and a
    /// `DROP TABLE` fires `ON DELETE` actions on child rows while enforcement is on (a rebuild of a table
    /// with `ON DELETE CASCADE` children would delete those rows). SQLite only allows `PRAGMA
    /// foreign_keys` to be toggled **outside** a transaction (it is a no-op within one), so enforcement
    /// is turned off before `BEGIN` and restored after the transaction ends; `PRAGMA foreign_key_check`
    /// re-validates referential integrity before the commit, so a genuine violation (e.g. a newly added
    /// or tightened foreign key whose existing data no longer satisfies it) still fails the batch
    /// instead of committing silently. The prior setting is read and restored (rather than forced on) so
    /// a caller that left enforcement off on a [`new`](SqliteConnection::new)-built handle keeps it off.
    async fn execute_ddl(&mut self, sql: &str) -> Result<(), SqliteError> {
        let sql = sql.to_owned();
        self.conn
            .call(move |conn| {
                // Read the current setting so it can be restored, then disable enforcement for the batch
                // (both toggled outside any transaction, where the pragma is a no-op).
                let was_on: bool =
                    conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))? != 0;
                conn.execute_batch("PRAGMA foreign_keys = OFF")?;
                let result = apply_ddl_batch(conn, &sql);
                // Restore the prior setting whatever happened (a failed batch has already rolled back).
                let restore = if was_on {
                    "PRAGMA foreign_keys = ON"
                } else {
                    "PRAGMA foreign_keys = OFF"
                };
                let restored = conn.execute_batch(restore);
                result.and(restored)
            })
            .await
            .map_err(SqliteError::Execute)
    }

    /// Refuses a plan that would silently destroy a view's `INSTEAD OF` triggers (git-bug 6a3940a).
    ///
    /// SQLite has no `CREATE OR REPLACE VIEW`, so every view change applies as `DROP VIEW` +
    /// `CREATE VIEW`, and a rebuild of a table under a view drops that view too (see
    /// [`plan_view_drops`](crate::sql::plan_view_drops)) — `DROP VIEW` takes the view's `INSTEAD OF`
    /// triggers with it, and squealy has no trigger model, so nothing recreates them. Writes through a
    /// writeable view then fail, with no step in the plan naming the view.
    ///
    /// This bites two paths the neutral risk classification calls safe: a view-body change (one
    /// `CreateView` step, classified `Safe` because it is `CREATE OR REPLACE` on PostgreSQL/MySQL), and
    /// a table rebuild forced by a change that is genuinely safe elsewhere (`ADD CHECK`/`ADD UNIQUE`,
    /// which SQLite cannot `ALTER ... ADD CONSTRAINT`), whose plan need not mention the view at all.
    /// So this refuses rather than reclassifies: the drop set is a rendering decision only this backend
    /// can compute, and the trigger is only visible in the live database.
    ///
    /// Preserving the triggers is NOT attempted. Replaying their captured DDL was tried and retired
    /// (PR #81): a trigger's real dependency surface is its body, so a replay cannot be validated
    /// without modelling triggers — a batch can rebuild a base table, leave the view byte-identical,
    /// and leave the trigger body referencing dropped columns. Refusing loudly is what this backend can
    /// honestly promise until triggers are modelled (epic acb1c6d Phase 3).
    ///
    /// The check runs **inside the batch's own transaction**, immediately before the DDL: checked in a
    /// separate round-trip, a concurrent connection could create a trigger after the check cleared the
    /// view and the batch would destroy it regardless — the silent loss this exists to prevent.
    async fn execute_plan_ddl(
        &mut self,
        sql: &str,
        plan: &DatabasePlan,
        desired: &DatabaseModel,
        allow_destructive: bool,
    ) -> Result<(), SqliteError> {
        // The drop set is pure (plan + desired), so compute it before entering the connection thread;
        // only the trigger lookup needs the live catalog, and that happens inside the transaction.
        let guarded: Vec<String> = if allow_destructive {
            Vec::new()
        } else {
            sql::plan_view_drops(plan, desired)
                .dropped()
                .map(|view| view.name.clone())
                .collect()
        };
        let sql = sql.to_owned();
        // A refusal is an outcome, not a SQLite failure: the closure yields the doomed trigger names
        // rather than an error, so the ordinary error type (and its mapping) is untouched.
        let refused = self
            .conn
            .call(move |conn| {
                let was_on: bool =
                    conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))? != 0;
                conn.execute_batch("PRAGMA foreign_keys = OFF")?;
                let result = apply_guarded_ddl_batch(conn, &sql, &guarded);
                let restore = if was_on {
                    "PRAGMA foreign_keys = ON"
                } else {
                    "PRAGMA foreign_keys = OFF"
                };
                let restored = conn.execute_batch(restore);
                // A batch failure outranks a failure to restore the pragma, as before.
                match result {
                    Err(error) => Err(error),
                    Ok(outcome) => restored.map(|()| outcome),
                }
            })
            .await
            .map_err(SqliteError::Execute)?;
        match refused {
            Some(triggers) => Err(SqliteError::DependentTriggers { triggers }),
            None => Ok(()),
        }
    }
}

/// [`apply_ddl_batch`], preceded by the view-trigger guard — in the same transaction, so a trigger
/// created concurrently after the guard cleared cannot be silently destroyed by the batch (another
/// writer cannot commit while this transaction holds its read, and a WAL snapshot conflict fails the
/// batch loudly rather than losing the trigger).
///
/// Returns `Ok(Some(triggers))` when the batch is REFUSED — no DDL ran, and dropping the transaction
/// rolls back the read. `Ok(None)` means the batch applied and committed. `guarded` is empty when the
/// caller allowed destructive changes, which makes this exactly [`apply_ddl_batch`].
fn apply_guarded_ddl_batch(
    conn: &mut rusqlite::Connection,
    sql: &str,
    guarded: &[String],
) -> rusqlite::Result<Option<Vec<String>>> {
    let transaction = conn.transaction()?;
    let doomed = view_triggers(&transaction, guarded)?;
    if !doomed.is_empty() {
        return Ok(Some(doomed));
    }
    transaction.execute_batch(sql)?;
    let has_violation = {
        let mut check = transaction.prepare("PRAGMA foreign_key_check")?;
        let mut rows = check.query([])?;
        rows.next()?.is_some()
    };
    if has_violation {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some("foreign key constraint violation after applying the schema change".to_owned()),
        ));
    }
    transaction.commit().map(|()| None)
}

/// The live triggers attached to any of `views`, as `trigger ON view` pairs — what a `DROP VIEW` of
/// those views would destroy.
///
/// Reads **both** trigger catalogs: a `TEMP` trigger on a main-schema view lives in
/// `sqlite_temp_master`, not `sqlite_master`, and `DROP VIEW` destroys it just the same (verified).
/// Temp objects are per-connection, so this sees exactly the ones this connection's `DROP VIEW` would
/// take. (PR #81 abandoned TEMP handling, but that was for *replaying* triggers, where a temp/main
/// same-name collision could resurrect one onto the wrong object; merely reporting what is about to be
/// destroyed carries no such ambiguity.)
///
/// Names are folded ASCII-only, as SQLite resolves identifiers and as `plan_view_drops` already does:
/// `"Ä"` and `"ä"` are *distinct* views (verified), so Unicode folding would refuse a plan over a
/// trigger belonging to an unrelated view.
fn view_triggers(conn: &rusqlite::Connection, views: &[String]) -> rusqlite::Result<Vec<String>> {
    if views.is_empty() {
        return Ok(Vec::new());
    }
    let wanted: Vec<String> = views.iter().map(|name| name.to_ascii_lowercase()).collect();
    let mut statement = conn.prepare(
        "SELECT name, tbl_name FROM sqlite_master WHERE type = 'trigger' \
         UNION ALL \
         SELECT name, tbl_name FROM sqlite_temp_master WHERE type = 'trigger' \
         ORDER BY name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut found = Vec::new();
    for row in rows {
        let (trigger, target) = row?;
        if wanted.contains(&target.to_ascii_lowercase()) {
            found.push(format!("{trigger} ON {target}"));
        }
    }
    Ok(found)
}

/// Applies a DDL batch inside a transaction, re-validating foreign keys before committing. The caller
/// disables enforcement for the batch (see [`DdlExecutor::execute_ddl`]), so referential integrity is
/// not checked statement-by-statement; `PRAGMA foreign_key_check` verifies it before the commit, and
/// any violation aborts the batch (the transaction rolls back on the early return).
fn apply_ddl_batch(conn: &mut rusqlite::Connection, sql: &str) -> rusqlite::Result<()> {
    let transaction = conn.transaction()?;
    transaction.execute_batch(sql)?;
    let has_violation = {
        let mut check = transaction.prepare("PRAGMA foreign_key_check")?;
        let mut rows = check.query([])?;
        rows.next()?.is_some()
    };
    if has_violation {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some("foreign key constraint violation after applying the schema change".to_owned()),
        ));
    }
    transaction.commit()
}

impl SchemaIntrospect for SqliteConnection {
    type Error = SqliteError;

    async fn introspect_database(&mut self) -> Result<DatabaseModel, SqliteError> {
        introspect::database(self).await
    }

    /// SQLite is dynamically typed: a column's declared type only assigns one of five affinities, so a
    /// logical type read back can only be its affinity's representative (e.g. `Varchar`/`Uuid`/`Bool`
    /// all read back as their affinity's type). Collapse a desired type to the same representative so it
    /// does not churn as an ambiguous type change against the live schema.
    fn canonical_sql_type(&self, ty: &SqlType) -> SqlType {
        // A fixed-width byte column keeps its width: SQLite enforces `[u8; N]` with a generated length
        // `CHECK` that introspection recovers as `FixedBytes(N)`, so — unlike other types — it must not
        // collapse to its `BLOB` affinity (`Bytes`), or a width change (or `FixedBytes` vs `Bytes`) would
        // not diff and would leave a stale check.
        if matches!(ty, SqlType::FixedBytes(_)) {
            return ty.clone();
        }
        introspect::representative_type(introspect::affinity_of_sql_type(ty))
    }

    /// SQLite cannot type a view's output columns: `PRAGMA table_info` reports no type for a computed
    /// output (`length(x)`, `a || b`), so introspection cannot recover a view column's type at all (it
    /// reads views back by name only). Collapse every desired view column to a single sentinel type so it
    /// matches what introspection produces — the diff then compares view columns by name alone. A
    /// column-set change (add/remove/rename) still differs and forces a destructive `DropView` +
    /// re-create; a pure output-type change (same names) is a no-op, which is correct for a view (it holds
    /// no data). Table columns keep their affinity via [`canonical_sql_type`](Self::canonical_sql_type);
    /// only view columns collapse this far.
    fn canonical_view_column_type(&self, _ty: &SqlType) -> SqlType {
        SqlType::Bytes
    }

    /// Reconciles a reconstructed view body's two SQLite-specific divergences so a published view whose
    /// body the reverse parser now recovers from `sqlite_master.sql` re-plans to empty. Applied to **both**
    /// the desired and the introspected model (see
    /// [`SchemaIntrospect::canonical_view_body`](squealy::SchemaIntrospect::canonical_view_body)):
    /// - **The suppressed schema qualifier.** SQLite has no namespaces, so it renders every view source
    ///   unqualified; the reconstructed body's every `SourceRef.schema` reads back `None`, while a
    ///   `from_database` desired body carries the mapped `Some("app")`. Flatten both sides to `None`
    ///   (mirrors [`canonical_schema_name`](Self::canonical_schema_name), which flattens the top-level
    ///   schema).
    /// - **The many-to-one result-pin casts.** A `CAST(<call> AS <type>)` pin renders with one of SQLite's
    ///   five affinity names, so several [`SqlType`]s collapse to one on the round trip; fold each pin to
    ///   that affinity's canonical representative (see [`canonical_sqlite_pin_type`]).
    /// - **The `LIKE` case-sensitivity flag.** SQLite renders both `case_insensitive` states as plain
    ///   `LIKE` (it has no `ILIKE`), which the reverse parser reads back as `false`; fold every
    ///   [`ExprNode::Like`](squealy::ExprNode::Like) in the body to `false` so an authored `ILIKE` does not
    ///   churn (mirrors [`canonical_check_expression`](Self::canonical_check_expression) /
    ///   [`canonical_index_predicate`](Self::canonical_index_predicate), which fold it on checks/predicates).
    fn canonical_view_body(&self, mut body: squealy::ViewBody) -> squealy::ViewBody {
        body.map_sources(&|source| source.schema = None);
        body.map_result_pins(&canonical_sqlite_pin_type);
        body.map_exprs(&|expr| match expr {
            squealy::ExprNode::Like {
                case_insensitive, ..
            } => {
                *case_insensitive = false;
            }
            // Fold a general cast in a view body to SQLite's canonical affinity representative, as a
            // table check's cast is folded, so a structural desired cast does not churn.
            squealy::ExprNode::Cast { ty, .. } => {
                *ty = canonical_sqlite_cast_type(ty);
            }
            _ => {}
        });
        body
    }

    fn canonical_cast_type(&self, ty: &SqlType) -> SqlType {
        canonical_sqlite_cast_type(ty)
    }

    /// SQLite resolves identifiers case-insensitively over ASCII only.
    fn identifier_case(&self) -> squealy::IdentifierCase {
        squealy::IdentifierCase::AsciiInsensitive
    }

    /// SQLite makes every CTE in a `WITH` mutually visible, so a CTE may forward-reference a later sibling.
    fn cte_forward_references_visible(&self) -> bool {
        true
    }

    /// SQLite's only identity mechanism is `AUTOINCREMENT`, which introspects back as
    /// [`IdentityMode::AutoIncrement`]. Map every declared mode to it so a crate-declared
    /// `auto_increment` column (which enters the model as `ByDefault`) does not churn as an ambiguous
    /// identity change.
    fn canonical_identity_mode(&self, _mode: &IdentityMode) -> IdentityMode {
        IdentityMode::AutoIncrement
    }

    /// SQLite has no namespaces: every table is rendered and introspected unqualified, so flatten a
    /// desired schema name to `None` to match.
    fn canonical_schema_name(&self, _name: Option<&str>) -> Option<String> {
        None
    }

    /// SQLite has no `CREATE SCHEMA`, so an empty namespace has nothing to create and introspection
    /// reports no schema for an empty database; canonicalization drops an empty flattened schema to
    /// match.
    fn has_namespaces(&self) -> bool {
        false
    }

    /// SQLite does not name a primary key; introspection reports it with an empty name, so flatten a
    /// crate-declared `pk_<table>` to match. A table has at most one primary key, so an empty name never
    /// collides.
    fn canonical_primary_key_name(&self, _name: &str) -> String {
        String::new()
    }

    /// SQLite does not round-trip a `UNIQUE` constraint's name (its backing auto-index is
    /// `sqlite_autoindex_…`). Derive a stable name from the constraint's columns — identical on the
    /// desired and introspected side — so equivalent uniques compare equal while staying distinct when a
    /// table has more than one.
    fn canonical_unique_name(&self, unique: &Constraint) -> String {
        format!("unique:{}", unique.columns.join(","))
    }

    /// SQLite reports foreign keys positionally with no name; derive a stable name from the referencing
    /// columns and the referenced table/columns (identical on both sides) so equivalent foreign keys
    /// compare equal. The referenced schema is already flattened to `None` by
    /// [`canonical_schema_name`](Self::canonical_schema_name), so it is not part of the key.
    fn canonical_foreign_key_name(&self, foreign_key: &ForeignKeyModel) -> String {
        format!(
            "foreign_key:{}->{}({})",
            foreign_key.columns.join(","),
            foreign_key.references_table,
            foreign_key.references_columns.join(","),
        )
    }

    /// SQLite renders a `CHECK` constraint inline and unnamed, and introspection recovers it by parsing
    /// the `CREATE TABLE` text — which yields only the expression, not a name. Derive a stable name from
    /// the structural expression (identical on the desired and introspected side) so equivalent checks
    /// compare equal while a table's several checks stay distinct. The name is only a diff key (SQLite
    /// emits checks unnamed), so the structural `Debug` form is a fine stable, unique key.
    fn canonical_check_name(&self, check: &CheckModel) -> String {
        format!("check:{:?}", check.expression)
    }

    /// Structures a `Raw` check expression (a legacy package's verbatim check, or an un-invertible
    /// introspected one) by re-parsing it in the SQLite dialect, so it compares equal to a freshly
    /// introspected structural check. An already-structural expression is returned unchanged.
    ///
    /// The raw SQL is trimmed first: SQLite introspection extracts a `CHECK` body from the stored
    /// `CREATE TABLE` text with surrounding whitespace stripped, so an un-lowerable raw check must trim
    /// too or a padded desired `" f(x) "` would not match the introspected `"f(x)"`.
    fn canonical_check_expression(&self, expression: squealy::ExprNode) -> squealy::ExprNode {
        let structured = match expression {
            squealy::ExprNode::Raw(sql) => {
                match squealy_parse::Reader::new(squealy_parse::SqlDialect::Sqlite)
                    .read_check_expression_or_raw(sql.trim())
                {
                    squealy::ExprNode::Raw(raw) => squealy::ExprNode::Raw(raw.trim().to_owned()),
                    structured => structured,
                }
            }
            other => other,
        };
        // SQLite renders both `Like` case-sensitivity states as plain `LIKE` and introspects them back as
        // `case_insensitive: false`, so fold the flag to keep an authored `ILIKE` check from churning.
        squealy::fold_like_case_insensitivity(&structured)
    }

    /// Structures a `Raw` partial-index predicate by re-parsing it in the SQLite dialect (the predicate is
    /// recovered from the stored `CREATE INDEX` text, so an un-lowerable one is trimmed), then folds the
    /// `Like` case-sensitivity flag — mirroring [`canonical_check_expression`](Self::canonical_check_expression).
    fn canonical_index_predicate(&self, predicate: squealy::ExprNode) -> squealy::ExprNode {
        let structured = match predicate {
            squealy::ExprNode::Raw(sql) => {
                squealy_parse::Reader::new(squealy_parse::SqlDialect::Sqlite)
                    .read_index_predicate_or_raw(sql.trim())
            }
            other => other,
        };
        squealy::fold_like_case_insensitivity(&structured)
    }

    /// SQLite has no boolean or unsigned literal, so a `bool`/unsigned default on an `INTEGER`-affinity
    /// column is rendered as a plain integer and reads back as [`DefaultValue::Int`]; likewise an integer
    /// default on a `REAL`-affinity column reads back as a float. Collapse the desired default the same
    /// way (using the already-canonicalized column type) so a defaulted column does not churn.
    fn canonical_default(&self, ty: &SqlType, default: &DefaultValue) -> DefaultValue {
        match introspect::affinity_of_sql_type(ty) {
            introspect::Affinity::Integer => match default {
                DefaultValue::Bool(value) => DefaultValue::Int(i128::from(*value)),
                // A `UInt` beyond `i128::MAX` (a large `u128`) has no `Int` representation; the renderer
                // writes it as a decimal literal and the introspector — whose `parse::<i128>()` also
                // overflows — reads it back as `Raw(text)`, so canonicalize to that same text.
                DefaultValue::UInt(value) => i128::try_from(*value)
                    .map(DefaultValue::Int)
                    .unwrap_or_else(|_| DefaultValue::Raw(value.to_string())),
                other => other.clone(),
            },
            introspect::Affinity::Real => match default {
                DefaultValue::Int(value) => DefaultValue::Float(*value as f64),
                other => other.clone(),
            },
            // A NUMERIC-affinity column (`Decimal`/`Raw("NUMERIC")`) has no structured numeric literal:
            // the renderer writes the value verbatim and introspection reads it back as `Raw(text)`
            // (SQLite does not preserve the logical numeric type), so collapse a structured numeric
            // default to the same rendered text.
            introspect::Affinity::Numeric => match default {
                DefaultValue::Int(value) => DefaultValue::Raw(value.to_string()),
                DefaultValue::UInt(value) => DefaultValue::Raw(value.to_string()),
                DefaultValue::Float(value) => DefaultValue::Raw(value.to_string()),
                DefaultValue::Bool(value) => {
                    DefaultValue::Raw(if *value { "1" } else { "0" }.to_owned())
                }
                other => other.clone(),
            },
            _ => default.clone(),
        }
    }
}

/// Folds a result-pin [`SqlType`](squealy::SqlType) to the canonical representative SQLite introspection
/// yields for it. A view body's `CAST(<call> AS <type>)` pin renders with one of SQLite's five affinity
/// names (`write_cast_type` == [`sqlite_affinity`](crate::sql::sqlite_affinity)), the lossiest cast
/// vocabulary of the three backends — every integer width **and `Bool`** collapse to `INTEGER`, every
/// text-like type to `TEXT`, both binary widths to `BLOB`, and any `Decimal` to bare `NUMERIC` — so the
/// stored DDL re-renders each pin with the same keyword the reverse parser (`invert_sqlite_cast_type` in
/// `squealy-parse`) inverts to one representative. This is exactly `invert_sqlite_cast_type(sqlite_affinity
/// (ty))`, written directly off `sqlite_affinity`'s groupings so mapping the desired side's narrower pin to
/// the same representative lets a published view re-plan to empty:
/// - `INTEGER` affinity → `I64`;
/// - `REAL` affinity → `F64`;
/// - `TEXT` affinity (incl. `Date`/`Time`/`Timestamp`/`Uuid`/`Json`/`Jsonb`) → `Text`;
/// - `BLOB` affinity → `Bytes` (a view *pin* renders bare `BLOB`, so — unlike a `FixedBytes` *column*,
///   whose width a generated length `CHECK` preserves — the pin fold collapses `FixedBytes` → `Bytes`;
///   this deliberately differs from [`canonical_sql_type`](SqliteConnection::canonical_sql_type));
/// - `NUMERIC` affinity → `Decimal { precision: 10, scale: 0 }` (the affinity drops the precision, which
///   `decimal_from_exact(None)` cannot recover);
/// - a `Raw` pin's affinity is its own text, which does not invert to a known type, so it is left unchanged.
fn canonical_sqlite_pin_type(ty: &SqlType) -> SqlType {
    use SqlType::{
        Bool, Bytes, Char, Date, Decimal, F32, F64, FixedBytes, I8, I16, I32, I64, I128, Isize,
        Json, Jsonb, Raw, String as SqlString, Text, Time, Timestamp, U8, U16, U32, U64, U128,
        Usize, Uuid, Varchar,
    };
    match ty {
        Bool | I8 | I16 | I32 | I64 | I128 | Isize | U8 | U16 | U32 | U64 | U128 | Usize => I64,
        F32 | F64 => F64,
        SqlString
        | Varchar(_)
        | Char(_)
        | Text
        | Date
        | Time { .. }
        | Timestamp { .. }
        | Uuid
        | Json
        | Jsonb => Text,
        Bytes | FixedBytes(_) => Bytes,
        Decimal { .. } => Decimal {
            precision: 10,
            scale: 0,
        },
        // A `Raw` pin's affinity is its own text, which `invert_sqlite_cast_type` maps to `None`; leave it.
        Raw(_) => ty.clone(),
        // An enum is never a SQLite pin type (enum columns are rejected at render); leave it unchanged.
        SqlType::Enum(_) => ty.clone(),
    }
}

/// Folds a **general** authored cast's target type for SQLite. Identical to [`canonical_sqlite_pin_type`]
/// except a 128-bit-integer cast is preserved (not folded to the `I64` affinity representative): SQLite
/// cannot spell it, so [`SqliteDialect::write_general_cast_type`](crate::sql::SqliteDialect) rejects it at
/// render — but the incremental plan path canonicalizes the model *before* rendering, so folding `I128` to
/// `I64` here would let a `CAST(x AS INTEGER)` render silently instead. Keeping it 128-bit makes the render
/// reject fire on both the direct and plan paths. A `Decimal` still folds to the `NUMERIC` representative
/// (also rejected at render regardless of its precision). See git-bug 8fe1530.
fn canonical_sqlite_cast_type(ty: &SqlType) -> SqlType {
    match ty {
        SqlType::I128 | SqlType::U128 => ty.clone(),
        other => canonical_sqlite_pin_type(other),
    }
}

impl SqliteConnection {
    /// Whether a `__squealy_*` bookkeeping table exists yet (the stores create their table lazily on the
    /// first write, so a read before any write returns empty rather than erroring on a missing table).
    async fn bookkeeping_table_exists(&self, table: &'static str) -> Result<bool, SqliteError> {
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |_| Ok(()),
                )
                .optional()
            })
            .await
            .map(|found| found.is_some())
            .map_err(SqliteError::Introspect)
    }
}

impl SchemaRefactorStore for SqliteConnection {
    type Error = SqliteError;

    async fn applied_refactor_ids(&mut self) -> Result<Vec<String>, SqliteError> {
        if !self.bookkeeping_table_exists("__squealy_refactors").await? {
            return Ok(Vec::new());
        }
        self.conn
            .call(|conn| {
                let mut statement =
                    conn.prepare("SELECT id FROM __squealy_refactors ORDER BY id")?;
                let ids = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(ids)
            })
            .await
            .map_err(SqliteError::Introspect)
    }

    async fn record_applied_refactor_ids(&mut self, ids: &[String]) -> Result<(), SqliteError> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids = ids.to_vec();
        self.conn
            .call(move |conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS __squealy_refactors (\
                     id TEXT NOT NULL PRIMARY KEY, \
                     applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
                )?;
                let transaction = conn.transaction()?;
                {
                    let mut statement = transaction
                        .prepare("INSERT OR IGNORE INTO __squealy_refactors (id) VALUES (?1)")?;
                    for id in &ids {
                        statement.execute([id])?;
                    }
                }
                transaction.commit()
            })
            .await
            .map_err(SqliteError::Execute)
    }
}

impl SchemaMetadataStore for SqliteConnection {
    type Error = SqliteError;

    async fn schema_metadata(&mut self) -> Result<Vec<(String, String)>, SqliteError> {
        if !self.bookkeeping_table_exists("__squealy_metadata").await? {
            return Ok(Vec::new());
        }
        self.conn
            .call(|conn| {
                let mut statement =
                    conn.prepare("SELECT name, value FROM __squealy_metadata ORDER BY name")?;
                let entries = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(entries)
            })
            .await
            .map_err(SqliteError::Introspect)
    }

    async fn record_schema_metadata(
        &mut self,
        entries: &[(String, String)],
    ) -> Result<(), SqliteError> {
        if entries.is_empty() {
            return Ok(());
        }
        let entries = entries.to_vec();
        self.conn
            .call(move |conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS __squealy_metadata (\
                     name TEXT NOT NULL PRIMARY KEY, \
                     value TEXT NOT NULL, \
                     updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
                )?;
                let transaction = conn.transaction()?;
                {
                    let mut statement = transaction.prepare(
                        "INSERT INTO __squealy_metadata (name, value) VALUES (?1, ?2) \
                         ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                    )?;
                    for (name, value) in &entries {
                        statement.execute(rusqlite::params![name, value])?;
                    }
                }
                transaction.commit()
            })
            .await
            .map_err(SqliteError::Execute)
    }
}

impl SchemaPublishHistoryStore for SqliteConnection {
    type Error = SqliteError;

    async fn schema_publish_history(
        &mut self,
        limit: usize,
    ) -> Result<Vec<SchemaPublishRecord>, SqliteError> {
        if limit == 0
            || !self
                .bookkeeping_table_exists("__squealy_publish_history")
                .await?
        {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.conn
            .call(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT mode, package_hash, package_format_version, \
                            strftime('%Y-%m-%dT%H:%M:%S', applied_at) \
                     FROM __squealy_publish_history ORDER BY id DESC LIMIT ?1",
                )?;
                let records = statement
                    .query_map([limit], |row| {
                        Ok(SchemaPublishRecord {
                            mode: row.get(0)?,
                            package_hash: row.get(1)?,
                            package_format_version: row.get(2)?,
                            applied_at: row.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(records)
            })
            .await
            .map_err(SqliteError::Introspect)
    }

    async fn record_schema_publish(
        &mut self,
        mode: &str,
        package_hash: &str,
        package_format_version: &str,
    ) -> Result<(), SqliteError> {
        let (mode, package_hash, package_format_version) = (
            mode.to_owned(),
            package_hash.to_owned(),
            package_format_version.to_owned(),
        );
        self.conn
            .call(move |conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS __squealy_publish_history (\
                     id INTEGER PRIMARY KEY AUTOINCREMENT, \
                     mode TEXT NOT NULL, \
                     package_hash TEXT NOT NULL, \
                     package_format_version TEXT NOT NULL, \
                     applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
                )?;
                conn.execute(
                    "INSERT INTO __squealy_publish_history \
                     (mode, package_hash, package_format_version) VALUES (?1, ?2, ?3)",
                    rusqlite::params![mode, package_hash, package_format_version],
                )?;
                Ok(())
            })
            .await
            .map_err(SqliteError::Execute)
    }
}

impl ConnectionWithTransaction for SqliteConnection {
    type Transaction<'conn>
        = SqliteTransaction<'conn>
    where
        Self: 'conn;

    // Both entry points share the same `BEGIN` / body / finish shape; they differ only in how the body
    // closure is called (`AsyncFnOnce` vs a boxed-future `FnOnce`), and the body future borrows the
    // transaction, so the two cannot be unified behind one generic helper. The transaction handle owns
    // its own clone of the connection (a handle to the *same* underlying database), so `&mut self` is
    // not frozen for the duration; correctness rests on `tokio-rusqlite` running every submitted closure
    // FIFO on the one owned thread, and on this method holding the connection exclusively.
    //
    // The drop-guard transaction is constructed *before* `BEGIN`: if the future is cancelled while
    // `BEGIN` is in flight, the guard still exists and rolls back (a no-op if `BEGIN` never ran). The
    // guard is only disarmed (via `finish_transaction`) after a `COMMIT`/`ROLLBACK` actually succeeds,
    // so a failed `COMMIT` leaves it armed to clean up.
    async fn transaction<'conn, T, F>(&'conn mut self, f: F) -> Result<T, SqliteError>
    where
        T: 'conn,
        F: for<'tx> AsyncFnOnce(&'tx mut Self::Transaction<'conn>) -> Result<T, SqliteError>
            + 'conn,
    {
        let mut transaction = SqliteTransaction::new(self.conn.clone());
        begin(&self.conn, &mut transaction).await?;
        let result = f(&mut transaction).await;
        finish_transaction(&self.conn, &mut transaction, result).await
    }

    async fn transaction_scoped<'conn, T, F>(&'conn mut self, f: F) -> Result<T, SqliteError>
    where
        T: Send + 'conn,
        F: for<'tx> FnOnce(
                &'tx mut Self::Transaction<'conn>,
            )
                -> Pin<Box<dyn Future<Output = Result<T, SqliteError>> + Send + 'tx>>
            + 'conn,
    {
        let mut transaction = SqliteTransaction::new(self.conn.clone());
        begin(&self.conn, &mut transaction).await?;
        let result = f(&mut transaction).await;
        finish_transaction(&self.conn, &mut transaction, result).await
    }
}

/// Issues `BEGIN`. On failure — e.g. the connection is already inside a transaction (another
/// `SqliteConnection` over a clone of the same handle started one) — the guard is disarmed before the
/// error propagates, so its drop does not roll back a transaction this call never started. If instead
/// the future is *cancelled* while `BEGIN` is in flight, the guard stays armed and rolls back any
/// transaction that did begin.
async fn begin(
    conn: &tokio_rusqlite::Connection,
    transaction: &mut SqliteTransaction<'_>,
) -> Result<(), SqliteError> {
    if let Err(error) = conn.call(|conn| conn.execute_batch("BEGIN")).await {
        transaction.finalize();
        return Err(SqliteError::Query(error));
    }
    Ok(())
}

/// Commits (on `Ok`) or rolls back (on `Err`) and propagates the body's result, disarming the
/// transaction's drop guard **only** when the `COMMIT`/`ROLLBACK` actually succeeds:
/// - A failed `COMMIT` (e.g. `SQLITE_BUSY`, which can leave the transaction open) returns the commit
///   error with the guard still armed, so the drop guard rolls back.
/// - A failed `ROLLBACK` keeps the original body error and leaves the guard armed to retry on drop.
async fn finish_transaction<T>(
    conn: &tokio_rusqlite::Connection,
    transaction: &mut SqliteTransaction<'_>,
    result: Result<T, SqliteError>,
) -> Result<T, SqliteError> {
    match result {
        Ok(value) => {
            conn.call(|conn| conn.execute_batch("COMMIT"))
                .await
                .map_err(SqliteError::Query)?;
            transaction.finalize();
            Ok(value)
        }
        Err(error) => {
            if conn
                .call(|conn| conn.execute_batch("ROLLBACK"))
                .await
                .is_ok()
            {
                transaction.finalize();
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use squealy::SqlType::{
        self, Bool, Bytes, Char, Date, Decimal, F32, F64, FixedBytes, I8, I16, I32, I64, I128,
        Isize, Json, Jsonb, Raw, String as SqlString, Text, Time, Timestamp, U8, U16, U32, U64,
        U128, Usize, Uuid, Varchar,
    };

    use super::canonical_sqlite_pin_type;

    #[test]
    fn canonical_sqlite_pin_type_collapses_each_affinity_group() {
        // Every integer width and `Bool` share the `INTEGER` affinity → fold to `I64`.
        for ty in [
            Bool, I8, I16, I32, I64, I128, Isize, U8, U16, U32, U64, U128, Usize,
        ] {
            assert_eq!(
                canonical_sqlite_pin_type(&ty),
                I64,
                "{ty:?} → INTEGER → I64"
            );
        }
        // `REAL` affinity → `F64`.
        for ty in [F32, F64] {
            assert_eq!(canonical_sqlite_pin_type(&ty), F64, "{ty:?} → REAL → F64");
        }
        // Every text-like type (including the temporal, `Uuid`, and JSON types) → `TEXT` → `Text`.
        for ty in [
            SqlString,
            Varchar(255),
            Char(36),
            Text,
            Date,
            Time {
                tz: false,
                precision: Some(6),
            },
            Timestamp {
                tz: true,
                precision: None,
            },
            Uuid,
            Json,
            Jsonb,
        ] {
            assert_eq!(canonical_sqlite_pin_type(&ty), Text, "{ty:?} → TEXT → Text");
        }
        // Both binary widths → `BLOB` → `Bytes` (a view pin renders bare `BLOB`, so a `FixedBytes` pin
        // collapses too — unlike a `FixedBytes` column, whose width a generated `CHECK` preserves).
        for ty in [Bytes, FixedBytes(16)] {
            assert_eq!(
                canonical_sqlite_pin_type(&ty),
                Bytes,
                "{ty:?} → BLOB → Bytes"
            );
        }
        // Any `Decimal` → bare `NUMERIC`, whose precision is unrecoverable → `Decimal { 10, 0 }`.
        assert_eq!(
            canonical_sqlite_pin_type(&Decimal {
                precision: 20,
                scale: 4,
            }),
            Decimal {
                precision: 10,
                scale: 0,
            },
        );
        // A `Raw` pin's affinity is its own text, which does not invert to a known type → left unchanged.
        assert_eq!(
            canonical_sqlite_pin_type(&Raw("MYTYPE".to_owned())),
            Raw("MYTYPE".to_owned()),
        );
    }

    #[test]
    fn canonical_sqlite_pin_type_is_idempotent() {
        let types: [SqlType; 12] = [
            Bool,
            U32,
            F32,
            SqlString,
            Uuid,
            Timestamp {
                tz: true,
                precision: Some(3),
            },
            FixedBytes(8),
            Bytes,
            Decimal {
                precision: 20,
                scale: 4,
            },
            Text,
            I128,
            Raw("MYTYPE".to_owned()),
        ];
        for ty in types {
            let once = canonical_sqlite_pin_type(&ty);
            assert_eq!(
                canonical_sqlite_pin_type(&once),
                once,
                "{ty:?} did not stabilize"
            );
        }
    }
}
