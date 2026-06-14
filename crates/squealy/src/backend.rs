use std::future::Future;
use std::io::{self, Write};

use crate::{DatabaseModel, DatabasePlan, IdentityMode, IndexMethod, SqlType, Table};

/// Backend-specific row cursor used while decoding a projected row.
pub trait RowReader: Sized {
    type Backend: Backend;

    fn read<T>(&mut self) -> Result<T, <Self::Backend as Backend>::Error>
    where
        T: Decode<Self::Backend>;
}

/// Decode a Rust value from a backend row reader.
pub trait Decode<B: Backend>: Sized {
    fn decode(row: &mut B::RowReader<'_>) -> Result<Self, B::Error>;
}

impl<B> Decode<B> for ()
where
    B: Backend,
{
    fn decode(_row: &mut B::RowReader<'_>) -> Result<Self, B::Error> {
        Ok(())
    }
}

/// Decode a nullable Rust value from a backend row reader.
///
/// Implementations return `None` when the next backend column is SQL `NULL`;
/// otherwise they decode and wrap the concrete value.
pub trait DecodeNullable<B: Backend>: Sized {
    fn decode_nullable(row: &mut B::RowReader<'_>) -> Result<Option<Self>, B::Error>;
}

macro_rules! impl_decode_nullable_via_option {
    ($($ty:ty),* $(,)?) => {
        $(impl<B> DecodeNullable<B> for $ty
        where
            B: Backend,
            Option<$ty>: Decode<B>,
        {
            fn decode_nullable(row: &mut B::RowReader<'_>) -> Result<Option<Self>, B::Error> {
                row.read::<Option<$ty>>()
            }
        })*
    };
}

impl_decode_nullable_via_option! {
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    f32,
    f64,
    String,
    bool,
}

/// Native `uuid` column support: a `uuid::Uuid` field can appear in a nullable column or in a
/// left-joined row, both of which require `DecodeNullable`. Resolves only on backends that provide
/// `Decode<B> for Option<uuid::Uuid>` (the PostgreSQL backend does, behind its own `uuid` feature).
#[cfg(feature = "uuid")]
impl_decode_nullable_via_option! { uuid::Uuid }

/// Backend-specific parameter cursor used while encoding bind values.
///
/// This is the encode-side mirror of [`RowReader`]: where a row reader pulls typed
/// values *out* of a backend row, a param writer pushes typed values *into* a backend
/// parameter list. Concrete writers may expose additional backend-private helpers (for
/// example, appending a typed `NULL`) used by that backend's own [`Encode`] impls.
pub trait ParamWriter: Sized {
    type Backend: Backend;

    fn write<T>(&mut self, value: &T) -> Result<(), <Self::Backend as Backend>::Error>
    where
        T: Encode<Self::Backend>;
}

/// Encode a Rust value into a backend parameter writer.
///
/// This is the mirror of [`Decode`]. Backends provide impls for the primitive types and
/// for `Option<T>` (nullability), exactly as they do for decoding; custom and native
/// types are added by implementing this trait for the backends that support them.
pub trait Encode<B: Backend> {
    fn encode(&self, out: &mut B::ParamWriter<'_>) -> Result<(), B::Error>;
}

impl<B> Encode<B> for ()
where
    B: Backend,
{
    fn encode(&self, _out: &mut B::ParamWriter<'_>) -> Result<(), B::Error> {
        Ok(())
    }
}

impl<B, T> Encode<B> for &T
where
    B: Backend,
    T: Encode<B> + ?Sized,
{
    fn encode(&self, out: &mut B::ParamWriter<'_>) -> Result<(), B::Error> {
        T::encode(self, out)
    }
}

/// Backend-specific DDL generation.
pub trait Backend: Sized {
    type Error;

    type RowReader<'row>: RowReader<Backend = Self>;

    /// Encode-side mirror of [`RowReader`](Self::RowReader).
    type ParamWriter<'param>: ParamWriter<Backend = Self>;

    /// The backend's native bound-parameter representation (e.g. `PostgresParam`). A literal or
    /// runtime value is encoded into one of these via [`Encode`] before being handed to the driver.
    type Param;

    /// Construct a [`ParamWriter`](Self::ParamWriter) that appends encoded parameters to `params`.
    /// The shared renderer uses this to encode a single literal into [`Self::Param`].
    fn param_writer(params: &mut Vec<Self::Param>) -> Self::ParamWriter<'_>;

    fn no_rows_error() -> Self::Error;

    /// Generate backend-specific SQL for a table.
    fn write_table(&self, table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()>;
}

/// Marker for backends whose dialect supports a `RETURNING` clause on data-modifying statements
/// (PostgreSQL). The `insert_returning`/`update_returning`/`delete_returning` builders require it, so
/// a backend that does not implement it (such as MySQL, which has no `RETURNING`) rejects those
/// queries at compile time rather than failing at runtime.
pub trait SupportsReturning: Backend {}

/// Backend schema-management capabilities that are supported for full DDL/introspection
/// round-trips.
///
/// A capability should be `true` only when the backend can render the metadata and read it back into
/// the neutral model after applying the DDL. Syntax-only support is not enough for schema
/// management, because it would make publish-then-introspect lose model facts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchemaCapabilities {
    pub constraints: ConstraintCapabilities,
    pub indexes: IndexCapabilities,
}

/// Constraint metadata capabilities, split by constraint kind because SQL backends often expose
/// validation/enforcement differently for foreign keys and checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConstraintCapabilities {
    pub foreign_key_match_type: bool,
    pub foreign_key_deferrability: bool,
    pub foreign_key_validation: bool,
    pub foreign_key_enforcement: bool,
    pub check_validation: bool,
    pub check_enforcement: bool,
}

/// Index metadata capabilities for features that are not uniformly available across SQL backends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexCapabilities {
    pub predicates: bool,
    pub expressions: bool,
    pub include_columns: bool,
    pub null_ordering: bool,
    pub collations: bool,
    pub operator_classes: bool,
}

/// Backend-specific DDL rendering driven by an owned [`DatabaseModel`].
///
/// This is the schema-management counterpart to [`Backend`]: it renders ordered, whole-database
/// create-from-scratch DDL from the neutral model (and, in future, incremental `ALTER` plans and
/// introspection). Backends implement it against the core model so the `squealy-model` engine can
/// drive deployment without depending on any backend. It supersedes [`Backend::write_table`], which
/// renders only a single table.
pub trait SchemaBackend {
    /// Reports backend-wide schema-management capabilities.
    ///
    /// The default is conservative: no optional metadata is considered round-trippable unless a
    /// backend opts in.
    fn capabilities(&self) -> SchemaCapabilities {
        SchemaCapabilities::default()
    }

    /// Renders ordered create-from-scratch DDL for the whole model into `writer`.
    ///
    /// Implementations emit namespaces, then tables in foreign-key dependency order, then indexes,
    /// then foreign keys as separate `ALTER TABLE … ADD CONSTRAINT` statements (so dependency cycles
    /// and ordering do not block creation).
    fn render_create(&self, model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()>;

    /// Renders an ordered incremental DDL plan into `writer`.
    ///
    /// Backends own the SQL generation for these neutral plan steps. The default is conservative so
    /// backend crates can opt into incremental planning deliberately.
    fn render_plan(&self, _plan: &DatabasePlan, _writer: &mut impl Write) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "backend does not support incremental schema plan rendering",
        ))
    }

    /// Renders an incremental plan whose index-creation steps should use the backend's concurrent,
    /// non-locking form (e.g. PostgreSQL `CREATE INDEX CONCURRENTLY`).
    ///
    /// The default delegates to [`render_plan`](Self::render_plan), so backends without a concurrent
    /// form render identically. Callers pair this with
    /// [`DdlExecutor::execute_ddl_unmanaged`](DdlExecutor::execute_ddl_unmanaged), since the
    /// concurrent form usually cannot run inside a transaction.
    fn render_plan_concurrent(
        &self,
        plan: &DatabasePlan,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        self.render_plan(plan, writer)
    }
}

/// Executes already-rendered DDL against a live connection.
///
/// This is the execution half of schema management ([`SchemaBackend`] is the rendering half). It is
/// deliberately minimal — one batch of statements — and lives on the connection because executing DDL
/// needs the live connection a backend owns. Implementations should run the batch atomically where
/// the backend supports transactional DDL.
pub trait DdlExecutor {
    type Error;

    /// Executes one or more `;`-separated DDL statements as a single batch.
    fn execute_ddl(&mut self, sql: &str) -> impl Future<Output = Result<(), Self::Error>>;

    /// Executes `;`-separated DDL statements that must run *outside* a managing transaction, one at a
    /// time (e.g. PostgreSQL `CREATE INDEX CONCURRENTLY`).
    ///
    /// The default delegates to [`execute_ddl`](Self::execute_ddl); backends that wrap `execute_ddl`
    /// in a transaction override this to run each statement without one.
    fn execute_ddl_unmanaged(
        &mut self,
        sql: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.execute_ddl(sql)
    }
}

/// Reads a live database schema into the neutral [`DatabaseModel`].
///
/// This is the introspection half of schema management. Backends implement it on their live
/// schema-management connection type, alongside [`DdlExecutor`], so the model engine can compare a
/// declared/package model with the database state without knowing backend catalog details.
pub trait SchemaIntrospect {
    type Error;

    /// Introspects the current database visible to this connection.
    fn introspect_database(&mut self) -> impl Future<Output = Result<DatabaseModel, Self::Error>>;

    /// Canonicalizes a logical [`SqlType`] to the form this backend's introspection produces for it.
    ///
    /// Some logical types are physically identical in a backend — PostgreSQL renders both
    /// [`SqlType::String`] and [`SqlType::Text`] as `text`, which introspects back as `String`. A
    /// desired model is canonicalized through this before being diffed against an introspected one,
    /// so such types do not produce spurious, never-settling type-change churn. The default is the
    /// identity, which suits backends that keep the logical types distinct (e.g. MySQL).
    fn canonical_sql_type(&self, ty: &SqlType) -> SqlType {
        ty.clone()
    }

    /// Canonicalizes a logical [`IdentityMode`] to the form this backend's introspection produces.
    ///
    /// A crate-declared `auto_increment` column enters the desired model as
    /// [`IdentityMode::ByDefault`], but a backend may render and read it back as a different mode —
    /// MySQL renders `AUTO_INCREMENT` and introspects [`IdentityMode::AutoIncrement`]. Without this
    /// the same primary key diffs as a never-settling identity change after every publish. The
    /// default is the identity, which suits backends whose introspection preserves the logical mode
    /// (e.g. PostgreSQL).
    fn canonical_identity_mode(&self, mode: &IdentityMode) -> IdentityMode {
        mode.clone()
    }

    /// Canonicalizes a primary-key constraint name to the form this backend's introspection reports.
    ///
    /// MySQL ignores the declared constraint name and always reports a table's primary key as
    /// `PRIMARY`, so a crate-declared `pk_<table>` would otherwise diff as a never-settling
    /// `AlterPrimaryKey` after every publish. A desired model is canonicalized through this before
    /// diffing. The default is the identity, which suits backends that preserve the declared name
    /// (e.g. PostgreSQL).
    fn canonical_primary_key_name(&self, name: &str) -> String {
        name.to_owned()
    }

    /// The index access method this backend's introspection reports for an index declared without an
    /// explicit method, or `None` if it leaves the method unset.
    ///
    /// A crate-declared index enters the desired model with `method: None` and empty `directions`,
    /// while both live backends read defaults back as an explicit method (e.g. `Some(BTree)`) and
    /// ASC directions. A desired model is canonicalized through this (filling an absent method and
    /// treating empty directions as all-ASC) before diffing, so a plain index does not produce a
    /// never-settling `AlterIndex` after publish. The default is `None` (leave the method unset).
    fn default_index_method(&self) -> Option<IndexMethod> {
        None
    }
}

/// Reads backend metadata about explicit schema refactors already recorded against a database.
///
/// Backends own the physical storage for this metadata. The core contract is intentionally narrow:
/// return stable refactor operation ids, so the management engine can reason about which package
/// refactors have already been observed by a target database.
pub trait SchemaRefactorStore {
    type Error;

    /// Returns recorded refactor operation ids in deterministic order.
    fn applied_refactor_ids(&mut self) -> impl Future<Output = Result<Vec<String>, Self::Error>>;

    /// Records refactor operation ids as applied, ignoring ids that already exist.
    fn record_applied_refactor_ids(
        &mut self,
        ids: &[String],
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

/// Reads and records backend metadata about schema-management state.
///
/// This is backend-owned storage for current Squealy management facts such as the last published
/// package format and content hash. It is separate from application schema introspection and should
/// be excluded from the neutral [`DatabaseModel`].
pub trait SchemaMetadataStore {
    type Error;

    /// Returns recorded metadata entries in deterministic key order.
    fn schema_metadata(
        &mut self,
    ) -> impl Future<Output = Result<Vec<(String, String)>, Self::Error>>;

    /// Records metadata entries, replacing existing values for the same keys.
    fn record_schema_metadata(
        &mut self,
        entries: &[(String, String)],
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

/// One append-only schema publish event recorded by a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaPublishRecord {
    pub mode: String,
    pub package_hash: String,
    pub package_format_version: String,
    pub applied_at: String,
}

/// Records append-only schema publish history.
///
/// This is distinct from [`SchemaMetadataStore`]: metadata is current state, while history is an
/// audit trail of successful publish operations.
pub trait SchemaPublishHistoryStore {
    type Error;

    /// Returns recent publish events, newest first.
    fn schema_publish_history(
        &mut self,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<SchemaPublishRecord>, Self::Error>>;

    /// Records one successful publish operation.
    fn record_schema_publish(
        &mut self,
        mode: &str,
        package_hash: &str,
        package_format_version: &str,
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

/// Opens a schema-management connection from a connection string.
///
/// Backends implement this so the management engine and CLI can `publish` from a URL without knowing
/// backend-specific connection details. The returned connection executes DDL via [`DdlExecutor`].
pub trait SchemaConnect {
    type Connection: DdlExecutor;
    type Error;

    fn connect(&self, url: &str) -> impl Future<Output = Result<Self::Connection, Self::Error>>;
}
