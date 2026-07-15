//! DDL management engine for squealy.
//!
//! The owned, backend-neutral schema model lives in the core crate (so backends can implement
//! [`SchemaBackend`] against it without depending on this engine). This crate adds the operations
//! over that model: `.sqz` package export/import, and create/script/publish deployment orchestration.
//! Heavier dependencies (KDL, zip) are isolated here, away from the query core.
//!
//! See `docs/ddl-management.md` for the design.

#![forbid(unsafe_code)]

mod package;
mod plan;
mod refactor;

pub mod diff;

pub use diff::{
    ChangeRisk, ClassifiedDatabaseDiffChange, DatabaseDiff, DatabaseDiffChange, DiffPolicy,
    DiffPolicyError, TableDiffChange, check_diff_policy, diff_models,
};
pub use package::{
    FORMAT_VERSION, PACKAGE_CONTENT_HASH_METADATA_KEY, PACKAGE_FORMAT_VERSION_METADATA_KEY,
    PackageError, SQUEALY_MODEL_VERSION_METADATA_KEY, from_kdl, package_content_hash,
    package_metadata, read_package, read_package_from, read_refactor_log,
    read_refactor_log_from_package, refactor_from_kdl, refactor_to_kdl, to_kdl, write_package,
    write_package_to, write_package_with_refactors, write_package_with_refactors_to,
};
pub use plan::{
    ClassifiedDatabasePlanStep, classified_plan_steps, plan_diff, plan_models,
    plan_models_with_refactors, plan_step_risk, table_plan_step_risk,
};
pub use refactor::{
    AppliedRefactorError, CastColumn, RefactorLog, RefactorOperation, RenameColumn, RenameTable,
    pending_refactors,
};
pub use squealy::{
    CheckModel, ColumnCapabilities, ColumnModel, Constraint, ConstraintCapabilities,
    ConstraintDeferrability, ConstraintEnforcement, ConstraintValidation, CteModel, DatabaseModel,
    DatabasePlan, DatabasePlanStep, DdlExecutor, DefaultValue, ExprNode, ForeignKeyAction,
    ForeignKeyMatch, ForeignKeyModel, IndexCapabilities, IndexCollation, IndexDirection,
    IndexMethod, IndexModel, IndexNullsOrder, IndexOperatorClass, IndexPrefixLength, LogicalOp,
    ProjectionItem, SchemaBackend, SchemaCapabilities, SchemaConnect, SchemaIntrospect,
    SchemaMetadataStore, SchemaModel, SchemaPublishHistoryStore, SchemaPublishRecord,
    SchemaRefactorStore, SourceItem, SourceRef, SqlType, TableModel, TablePlanStep, ViewBody,
    ViewColumnModel, ViewModel, ViewQueryModel, ViewSetOp,
};

use std::collections::BTreeSet;

use squealy::Database;

/// Renders create-from-scratch DDL for an owned model using the given backend (the "script" /
/// dry-run operation: it produces SQL without touching a database).
pub fn render_create_sql<B: SchemaBackend>(
    model: &DatabaseModel,
    backend: &B,
) -> std::io::Result<String> {
    check_create(model, backend)?;
    let mut buffer = Vec::new();
    backend.render_create(model, &mut buffer)?;
    // SchemaBackend renderers are expected to emit UTF-8; surface a clear error rather than panicking
    // if a backend ever violates that invariant.
    bytes_to_sql(buffer)
}

/// Renders incremental DDL for a policy-checked [`DatabasePlan`].
///
/// `desired` is the target model the plan reaches; backends that rebuild whole tables (SQLite) need it
/// to render a rebuilt table's unchanged columns, which the per-change plan steps do not carry.
pub fn render_plan_sql<B: SchemaBackend>(
    plan: &DatabasePlan,
    desired: &DatabaseModel,
    backend: &B,
) -> std::io::Result<String> {
    // The incremental path does not run `check_create`, but a constraint's column prefix lengths can only
    // be validated against the referenced column types, which the per-change plan steps do not carry —
    // `desired` does. Validate them here (neutral capability + shape, then the backend's column type/width
    // rules) so an incremental `AddUnique`/`AddPrimaryKey` cannot slip an unrenderable/non-round-tripping
    // prefix past the create preflight into `render`/`apply`.
    let capabilities = backend.capabilities();
    for schema in &desired.schemas {
        for table in &schema.tables {
            validate_table_constraint_prefixes(table, &capabilities)?;
        }
    }
    validate_backend_constraint_prefixes(desired, backend)?;
    let mut buffer = Vec::new();
    backend.render_plan(plan, desired, &mut buffer)?;
    bytes_to_sql(buffer)
}

/// Renders incremental DDL exactly as [`apply_plan_with_options`] would execute it, so a dry-run
/// report reflects the real publish.
///
/// Without `concurrent_indexes` this is byte-identical to [`render_plan_sql`]. With it, index-add
/// steps are split out and rendered in the backend's concurrent form (PostgreSQL
/// `CREATE INDEX CONCURRENTLY`) after the transactional steps, under a comment marking that those
/// statements run outside the transaction — matching what `apply_plan_with_options` actually applies.
pub fn render_plan_with_options<B: SchemaBackend>(
    plan: &DatabasePlan,
    desired: &DatabaseModel,
    backend: &B,
    options: PlanApplyOptions,
) -> std::io::Result<String> {
    if !options.concurrent_indexes || !backend.supports_concurrent_index_creation() {
        return render_plan_sql(plan, desired, backend);
    }

    let (transactional, concurrent) = split_concurrent_index_steps(plan);
    let mut sql = if transactional.is_empty() {
        String::new()
    } else {
        render_plan_sql(&transactional, desired, backend)?
    };
    if !concurrent.is_empty() {
        let mut buffer = Vec::new();
        backend.render_plan_concurrent(&concurrent, desired, &mut buffer)?;
        let concurrent_sql = bytes_to_sql(buffer)?;
        if !sql.is_empty() {
            sql.push('\n');
        }
        sql.push_str("-- Applied outside the transaction (one statement per round-trip):\n");
        sql.push_str(&concurrent_sql);
    }
    Ok(sql)
}

/// Converts rendered DDL bytes to a `String`, returning an `InvalidData` error instead of panicking
/// if a backend renderer ever emits non-UTF-8 (the [`SchemaBackend`] contract forbids it).
fn bytes_to_sql(buffer: Vec<u8>) -> std::io::Result<String> {
    String::from_utf8(buffer)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Checks whether `backend` can create `model` without rendering or connecting to a database.
///
/// This validates backend capabilities against the neutral model, so callers can fail fast when a
/// package contains metadata the target backend cannot round-trip.
pub fn check_create<B: SchemaBackend>(model: &DatabaseModel, backend: &B) -> std::io::Result<()> {
    validate_capabilities(model, backend.capabilities())?;
    validate_backend_constraint_prefixes(model, backend)
}

/// Dispatches the backend-specific column type/width validation of constraint prefix lengths (see
/// [`SchemaBackend::validate_constraint_prefixes`]) over every table. The neutral capability + shape
/// checks run in [`validate_capabilities`]; this is the backend-owned half, shared by [`check_create`]
/// and [`render_plan_sql`].
fn validate_backend_constraint_prefixes<B: SchemaBackend>(
    model: &DatabaseModel,
    backend: &B,
) -> std::io::Result<()> {
    for schema in &model.schemas {
        for table in &schema.tables {
            backend.validate_constraint_prefixes(table)?;
        }
    }
    Ok(())
}

/// Renders create-from-scratch DDL straight from a compile-time [`Database`].
///
/// Equivalent to `render_create_sql(&DatabaseModel::from_database::<D>(), backend)`.
pub fn script<D: Database, B: SchemaBackend>(backend: &B) -> std::io::Result<String> {
    render_create_sql(&DatabaseModel::from_database::<D>(), backend)
}

/// An error from [`publish`]: either rendering the DDL or executing it failed.
#[derive(Debug, thiserror::Error)]
pub enum PublishError<E> {
    #[error("failed to render DDL: {0}")]
    Render(#[source] std::io::Error),
    #[error("failed to execute DDL: {0}")]
    Execute(#[source] E),
}

/// An error from [`plan_from_database`]: either introspection or policy checking failed.
#[derive(Debug, thiserror::Error)]
pub enum PlanFromDatabaseError<E> {
    #[error("failed to introspect database: {0}")]
    Introspect(#[source] E),
    #[error("failed to read applied refactors: {0}")]
    ReadAppliedRefactors(#[source] E),
    #[error("applied refactor metadata mismatch: {0}")]
    AppliedRefactor(#[source] AppliedRefactorError),
    #[error("schema plan blocked by policy: {0}")]
    Policy(#[source] DiffPolicyError),
}

/// The result of repairing backend refactor metadata from a package refactor log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefactorRepairReport {
    /// Refactor ids inserted into backend metadata during this repair.
    pub recorded: Vec<String>,
    /// Refactor ids that were already present in backend metadata.
    pub already_recorded: Vec<String>,
}

/// An error from [`repair_refactor_metadata`].
#[derive(Debug, thiserror::Error)]
pub enum RepairRefactorMetadataError<E> {
    #[error("failed to introspect database: {0}")]
    Introspect(#[source] E),
    #[error("failed to read applied refactors: {0}")]
    ReadAppliedRefactors(#[source] E),
    #[error("refactor final state mismatch: {0}")]
    AppliedRefactor(#[source] AppliedRefactorError),
    #[error("failed to record applied refactors: {0}")]
    RecordAppliedRefactors(#[source] E),
}

/// Renders create-from-scratch DDL for `model` and executes it against `connection`.
///
/// The backend executes the batch atomically where it supports transactional DDL, so a failed
/// create-from-scratch leaves no partial schema behind.
pub async fn publish<B, C>(
    model: &DatabaseModel,
    backend: &B,
    connection: &mut C,
) -> Result<(), PublishError<C::Error>>
where
    B: SchemaBackend,
    C: DdlExecutor,
{
    let sql = render_create_sql(model, backend).map_err(PublishError::Render)?;
    connection
        .execute_ddl(&sql)
        .await
        .map_err(PublishError::Execute)
}

/// Introspects `connection` and builds an incremental plan from the live model to `desired`.
pub async fn plan_from_database<C>(
    desired: &DatabaseModel,
    connection: &mut C,
    policy: DiffPolicy,
) -> Result<DatabasePlan, PlanFromDatabaseError<C::Error>>
where
    C: SchemaIntrospect,
{
    let actual = introspect(connection)
        .await
        .map_err(PlanFromDatabaseError::Introspect)?;
    // Canonicalize both sides so equivalent partial-index predicates / CHECK expressions written one
    // way and deparsed another compare equal (and so the desired model aligns with introspection).
    let actual = canonicalize_model(connection, &actual);
    let desired = canonicalize_model(connection, desired);
    plan_models(&desired, &actual, policy).map_err(PlanFromDatabaseError::Policy)
}

/// Introspects `connection` and builds an incremental plan using explicit refactor intent.
pub async fn plan_from_database_with_refactors<C>(
    desired: &DatabaseModel,
    refactors: &RefactorLog,
    connection: &mut C,
    policy: DiffPolicy,
) -> Result<DatabasePlan, PlanFromDatabaseError<<C as SchemaIntrospect>::Error>>
where
    C: SchemaIntrospect + SchemaRefactorStore<Error = <C as SchemaIntrospect>::Error>,
{
    let actual = introspect(connection)
        .await
        .map_err(PlanFromDatabaseError::Introspect)?;
    let applied_ids = connection
        .applied_refactor_ids()
        .await
        .map_err(PlanFromDatabaseError::ReadAppliedRefactors)?;
    // Canonicalize the refactor schema names so they match the live model a schema-less backend reads
    // back (introspected under the flattened namespace), and the flattened diff the steps drive.
    let refactors = canonicalize_refactors(connection, refactors);
    let pending_refactors = pending_refactors(&refactors, &applied_ids, &actual)
        .map_err(PlanFromDatabaseError::AppliedRefactor)?;
    // Canonicalize both sides for the diff (after refactor matching, which reads the raw schema).
    let actual = canonicalize_model(connection, &actual);
    let desired = canonicalize_model(connection, desired);
    plan_models_with_refactors(&desired, &actual, &pending_refactors, policy)
        .map_err(PlanFromDatabaseError::Policy)
}

/// Returns a copy of `model` in a canonical form for diffing, so a model does not churn against a
/// live schema. Column types go through [`SchemaIntrospect::canonical_sql_type`], identity modes
/// through [`SchemaIntrospect::canonical_identity_mode`], an index declared without an explicit
/// method / directions has them filled to the backend default (see
/// [`SchemaIntrospect::default_index_method`], empty directions becoming all-ASC), partial-index
/// predicates go through [`SchemaIntrospect::canonical_index_predicate`], index-key expressions
/// through [`SchemaIntrospect::canonical_index_expression`], and CHECK expressions
/// through [`SchemaIntrospect::canonical_check_expression`] (then names through
/// [`SchemaIntrospect::canonical_check_name`]).
///
/// The type/identity/method steps align a *desired* model with the form introspection produces; the
/// predicate/CHECK steps put an expression into a backend-neutral canonical form. [`plan_from_database`]
/// applies this to **both** the desired and the introspected model before diffing, so equivalent
/// expressions written one way and deparsed another compare equal. It is exposed so callers that
/// diff against a live schema directly (e.g. `status --check-schema`) can align both sides the same.
pub fn canonicalize_model<C: SchemaIntrospect>(
    connection: &C,
    model: &DatabaseModel,
) -> DatabaseModel {
    let default_method = connection.default_index_method();
    let mut model = model.clone();
    for schema in &mut model.schemas {
        // Flatten the namespace for a backend without schemas (SQLite), so a `#[schema(App)]` model does
        // not diff as a wholesale move of every table into the default namespace after each publish.
        schema.name = connection.canonical_schema_name(schema.name.as_deref());
        for table in &mut schema.tables {
            for column in &mut table.columns {
                column.ty = connection.canonical_sql_type(&column.ty);
                if let Some(default) = &mut column.default {
                    // Canonicalize the default against the (already-canonicalized) column type, so a
                    // default whose representation collapses with the type (SQLite renders a `bool`
                    // default as an integer) does not churn as an `AlterColumn` after publish.
                    *default = connection.canonical_default(&column.ty, default);
                }
                if let Some(identity) = &mut column.identity {
                    identity.mode = connection.canonical_identity_mode(&identity.mode);
                }
                if let Some(generated) = &mut column.generated
                    && let Some(expression) = generated.expression.take()
                {
                    // Structure a `Raw` generated expression (a legacy-package one, or an un-invertible
                    // introspected deparse) in the backend's dialect, then normalize it, so a generated
                    // column written one way and deparsed another compares equal instead of churning.
                    let expression = connection.canonical_generated_expression(expression);
                    let mut expression = squealy::normalize_expr(&expression);
                    // Fold each general cast's target to the backend's canonical representative (both
                    // sides), so a structural desired cast does not churn against the introspected form.
                    squealy::map_cast_types(&mut expression, &|ty| {
                        connection.canonical_cast_type(ty)
                    });
                    generated.expression = Some(expression);
                }
            }
            if let Some(primary_key) = &mut table.primary_key {
                primary_key.name = connection.canonical_primary_key_name(&primary_key.name);
            }
            for unique in &mut table.uniques {
                unique.name = connection.canonical_unique_name(unique);
            }
            for foreign_key in &mut table.foreign_keys {
                // Flatten a cross-schema reference the same way the referenced table's own schema name is
                // flattened, then canonicalize the (possibly non-round-tripping) constraint name.
                foreign_key.references_schema =
                    connection.canonical_schema_name(foreign_key.references_schema.as_deref());
                foreign_key.name = connection.canonical_foreign_key_name(foreign_key);
                // `NO ACTION` is the SQL default referential action; introspectors report an unset
                // action as `None`, so normalize an explicit `Some(NoAction)` to `None` on both sides so
                // a foreign key that spells out the default does not churn as an `AlterForeignKey`.
                normalize_no_action(&mut foreign_key.on_delete);
                normalize_no_action(&mut foreign_key.on_update);
            }
            for index in &mut table.indexes {
                // Structure each `Raw` term (a legacy-package index expression, or an un-invertible
                // introspected one) in the backend's dialect, then normalize it, so a legacy package's
                // expression index compares equal to a freshly introspected structural one. A single legacy
                // `Raw` may re-split into several structural terms, so this rebuilds the vector. This runs
                // BEFORE `canonicalize_index`, which sizes the default direction list by the (columns +
                // expressions) term count — a stale pre-split count would leave too few directions and churn.
                index.expressions = std::mem::take(&mut index.expressions)
                    .into_iter()
                    .flat_map(|expression| connection.canonical_index_expression(expression))
                    .map(|expression| {
                        let mut expression = squealy::normalize_expr(&expression);
                        squealy::map_cast_types(&mut expression, &|ty| {
                            connection.canonical_cast_type(ty)
                        });
                        expression
                    })
                    .collect();
                canonicalize_index(index, &default_method);
                if let Some(predicate) = index.predicate.take() {
                    // Structure a `Raw` predicate (a legacy-package partial index, or an un-invertible
                    // introspected one) in the backend's dialect, then normalize it, so a partial index
                    // written one way and deparsed another compares equal instead of churning.
                    let predicate = connection.canonical_index_predicate(*predicate);
                    let mut predicate = squealy::normalize_expr(&predicate);
                    squealy::map_cast_types(&mut predicate, &|ty| {
                        connection.canonical_cast_type(ty)
                    });
                    index.predicate = Some(Box::new(predicate));
                }
            }
            for check in &mut table.checks {
                // Structure a `Raw` expression (a legacy-package check, or an un-invertible introspected
                // one) in the backend's dialect so equivalent checks compare structurally; an already
                // structural expression is returned unchanged.
                check.expression = connection.canonical_check_expression(check.expression.clone());
                // Normalize the structural form (expand `BETWEEN`, re-nest boolean chains) so a check
                // PostgreSQL's deparse rewrites still compares equal to the authored one.
                check.expression = squealy::normalize_expr(&check.expression);
                // Fold each general cast's target to the backend's canonical representative (both sides),
                // so a structural desired cast does not churn against the introspected representative.
                squealy::map_cast_types(&mut check.expression, &|ty| {
                    connection.canonical_cast_type(ty)
                });
                // Derive the canonical name from that expression, so a backend that does not round-trip a
                // check's name (SQLite) matches equivalent checks by expression.
                check.name = connection.canonical_check_name(check);
            }
        }
        // A view's declared output columns are compared against introspected ones (which come back in
        // the backend's physical types), so canonicalize them the same way table columns are —
        // otherwise an unchanged view whose column type has a physical/logical alias (MySQL
        // `String`/`Varchar(255)`, PostgreSQL `Text`/`String`) would churn a drop+recreate every run.
        for view in &mut schema.views {
            for column in &mut view.columns {
                column.ty = connection.canonical_view_column_type(&column.ty);
                // A view's DDL carries no per-column NOT NULL, and introspection cannot reliably recover
                // nullability (PostgreSQL reports view outputs as nullable regardless of the underlying
                // column), so it is not a distinguishing feature of a view. Normalize it to one value on
                // both sides — otherwise a reconstructed view body, now compared structurally, would churn
                // on a nullability difference (a non-null underlying column vs the nullable introspected
                // output). This generalizes the body-unknown branch, which already compares view columns
                // ignoring nullability (see `diff_models`).
                column.nullable = true;
            }
            // Fold the reconstructed body's result-pin types to the backend's canonical representative
            // (many cast spellings are many-to-one), so a published view whose body is now reconstructed
            // by the reverse parser compares equal to its introspected form instead of churning.
            view.query = connection.canonical_view_body(std::mem::take(&mut view.query));
        }
    }
    // A schema-less backend flattens every namespace to the same (default) name, so two source schemas
    // can now share a name. `diff_models` keys schemas by name in a `BTreeMap`, which would drop the
    // tables/views of all but one same-named schema; coalesce them here (concatenating in first-seen
    // order) so the flattened model keeps every object. This is a no-op when names stay distinct.
    coalesce_schemas_by_name(&mut model.schemas);
    // A backend without namespace objects (SQLite) reports no schema for an empty database, so drop a
    // schema left with no tables or views — otherwise a desired model carrying an empty namespace diffs
    // as a spurious `CreateSchema` on every run. A backend with real schemas keeps them.
    if !connection.has_namespaces() {
        model
            .schemas
            .retain(|schema| !schema.tables.is_empty() || !schema.views.is_empty());
    }
    model
}

/// Merges schemas that share a name (after canonicalization) into the first one seen, concatenating
/// their tables and views. Needed for a schema-less backend, whose `canonical_schema_name` flattens
/// every namespace to `None`; a no-op when every schema name is already distinct.
fn coalesce_schemas_by_name(schemas: &mut Vec<SchemaModel>) {
    let mut coalesced: Vec<SchemaModel> = Vec::with_capacity(schemas.len());
    for schema in std::mem::take(schemas) {
        match coalesced
            .iter_mut()
            .find(|existing| existing.name == schema.name)
        {
            Some(existing) => {
                existing.tables.extend(schema.tables);
                existing.views.extend(schema.views);
            }
            None => coalesced.push(schema),
        }
    }
    *schemas = coalesced;
}

/// Returns a copy of `refactors` with each operation's schema name canonicalized the same way
/// [`canonicalize_model`] flattens namespaces, so refactor matching against the live model and the
/// rename/cast steps it drives line up with a schema-less backend's flattened schema. A no-op for a
/// backend with real schemas (the default [`SchemaIntrospect::canonical_schema_name`] is the identity).
fn canonicalize_refactors<C: SchemaIntrospect>(
    connection: &C,
    refactors: &RefactorLog,
) -> RefactorLog {
    let operations = refactors
        .operations
        .iter()
        .map(|operation| match operation {
            RefactorOperation::RenameTable(operation) => {
                RefactorOperation::RenameTable(RenameTable {
                    schema: connection.canonical_schema_name(operation.schema.as_deref()),
                    ..operation.clone()
                })
            }
            RefactorOperation::RenameColumn(operation) => {
                RefactorOperation::RenameColumn(RenameColumn {
                    schema: connection.canonical_schema_name(operation.schema.as_deref()),
                    ..operation.clone()
                })
            }
            RefactorOperation::CastColumn(operation) => RefactorOperation::CastColumn(CastColumn {
                schema: connection.canonical_schema_name(operation.schema.as_deref()),
                ..operation.clone()
            }),
        })
        .collect();
    RefactorLog { operations }
}

/// Normalizes an explicit `Some(ForeignKeyAction::NoAction)` to `None` (the referential-action default,
/// which every backend's introspection reports as an unset action).
fn normalize_no_action(action: &mut Option<ForeignKeyAction>) {
    if matches!(action, Some(ForeignKeyAction::NoAction)) {
        *action = None;
    }
}

/// Aligns an index's method / directions with the form the backend's introspection reads back, so a
/// plain crate-declared index does not churn as a never-settling `AlterIndex`.
///
/// The two directions are symmetric, keyed on whether the backend fills default index metadata:
/// - A backend that reports a default method (PostgreSQL, MySQL) also reads an unset index back with an
///   explicit method and all-ascending directions, so an index declared without them is *filled*.
/// - A backend that leaves the method unset (SQLite, the trait default) omits the default (`ASC`) sort
///   order, so trailing `Asc` directions are *trimmed*: an all-ascending list becomes empty, and a list
///   that specifies only a non-default prefix (e.g. `[Desc]` for two columns) matches the read-back
///   `[Desc, Asc]` trimmed back to `[Desc]`. A non-default direction (or one before a non-default) is
///   kept.
fn canonicalize_index(index: &mut IndexModel, default_method: &Option<IndexMethod>) {
    match default_method {
        Some(default_method) => {
            if index.method.is_none() {
                index.method = Some(default_method.clone());
            }
            if index.directions.is_empty() {
                let terms = index.columns.len() + index.expressions.len();
                index.directions = vec![IndexDirection::Asc; terms];
            }
        }
        None => {
            while index.directions.last() == Some(&IndexDirection::Asc) {
                index.directions.pop();
            }
        }
    }
}

/// Records package refactors as applied when the live schema already reflects their final state.
///
/// This repairs backend metadata only. It does not execute application-schema DDL.
pub async fn repair_refactor_metadata<C>(
    refactors: &RefactorLog,
    connection: &mut C,
) -> Result<RefactorRepairReport, RepairRefactorMetadataError<<C as SchemaIntrospect>::Error>>
where
    C: SchemaIntrospect + SchemaRefactorStore<Error = <C as SchemaIntrospect>::Error>,
{
    let actual = introspect(connection)
        .await
        .map_err(RepairRefactorMetadataError::Introspect)?;
    let applied_ids = connection
        .applied_refactor_ids()
        .await
        .map_err(RepairRefactorMetadataError::ReadAppliedRefactors)?;
    // Match the refactors against the live model under the same (possibly flattened) namespace it was
    // introspected in, so a schema-qualified refactor is validated against a schema-less backend.
    let refactors = &canonicalize_refactors(connection, refactors);
    // Casts are idempotent rendering hints, never recorded as applied refactors — recording one
    // would make `pending_refactors` filter it out and silently drop its `USING` clause.
    let package_ids = refactors
        .operations
        .iter()
        .filter(|operation| operation.is_recorded())
        .map(|operation| operation.id().to_owned())
        .collect::<Vec<_>>();

    pending_refactors(refactors, &package_ids, &actual)
        .map_err(RepairRefactorMetadataError::AppliedRefactor)?;

    let applied = applied_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut recorded = Vec::new();
    let mut already_recorded = Vec::new();
    for id in package_ids {
        if applied.contains(id.as_str()) {
            already_recorded.push(id);
        } else {
            recorded.push(id);
        }
    }

    if !recorded.is_empty() {
        connection
            .record_applied_refactor_ids(&recorded)
            .await
            .map_err(RepairRefactorMetadataError::RecordAppliedRefactors)?;
    }

    Ok(RefactorRepairReport {
        recorded,
        already_recorded,
    })
}

/// Options controlling how [`apply_plan_with_options`] executes a plan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanApplyOptions {
    /// Create indexes with the backend's concurrent, non-locking form outside the transaction
    /// (PostgreSQL `CREATE INDEX CONCURRENTLY`). Index-add steps are applied after the transactional
    /// steps, each on its own connection round-trip, so they are not part of the atomic batch.
    pub concurrent_indexes: bool,
}

/// Renders `plan` using `backend` and executes it against `connection`.
///
/// `desired` is the target model the plan reaches; it is forwarded to
/// [`render_plan_sql`] so table-rebuild backends (SQLite) can render a rebuilt table's unchanged
/// columns.
pub async fn apply_plan<B, C>(
    plan: &DatabasePlan,
    desired: &DatabaseModel,
    backend: &B,
    connection: &mut C,
) -> Result<(), PublishError<C::Error>>
where
    B: SchemaBackend,
    C: DdlExecutor,
{
    apply_plan_with_options(
        plan,
        desired,
        backend,
        connection,
        PlanApplyOptions::default(),
    )
    .await
}

/// Renders and executes `plan`, honouring [`PlanApplyOptions`].
///
/// With `concurrent_indexes`, index-add steps are split out and applied after the transactional
/// steps using the backend's concurrent index form outside a transaction. This trades the plan's
/// all-or-nothing guarantee (a concurrent index can fail independently, leaving an invalid index to
/// drop and retry) for not locking the table against writes while the index builds.
pub async fn apply_plan_with_options<B, C>(
    plan: &DatabasePlan,
    desired: &DatabaseModel,
    backend: &B,
    connection: &mut C,
    options: PlanApplyOptions,
) -> Result<(), PublishError<C::Error>>
where
    B: SchemaBackend,
    C: DdlExecutor,
{
    if plan.is_empty() {
        return Ok(());
    }
    if !options.concurrent_indexes || !backend.supports_concurrent_index_creation() {
        let sql = render_plan_sql(plan, desired, backend).map_err(PublishError::Render)?;
        return connection
            .execute_ddl(&sql)
            .await
            .map_err(PublishError::Execute);
    }

    let (transactional, concurrent) = split_concurrent_index_steps(plan);
    if !transactional.is_empty() {
        let sql =
            render_plan_sql(&transactional, desired, backend).map_err(PublishError::Render)?;
        connection
            .execute_ddl(&sql)
            .await
            .map_err(PublishError::Execute)?;
    }
    if !concurrent.is_empty() {
        let mut buffer = Vec::new();
        backend
            .render_plan_concurrent(&concurrent, desired, &mut buffer)
            .map_err(PublishError::Render)?;
        let sql = bytes_to_sql(buffer).map_err(PublishError::Render)?;
        connection
            .execute_ddl_unmanaged(&sql)
            .await
            .map_err(PublishError::Execute)?;
    }
    Ok(())
}

/// Splits a plan into its transactional steps and its index-add steps (which can be created
/// concurrently), preserving order within each group. Index additions sort after the transactional
/// steps that may create the columns they reference.
fn split_concurrent_index_steps(plan: &DatabasePlan) -> (DatabasePlan, DatabasePlan) {
    let mut transactional = Vec::new();
    let mut concurrent = Vec::new();
    for step in &plan.steps {
        let is_add_index = matches!(
            step,
            DatabasePlanStep::AlterTable { change, .. }
                if matches!(change.as_ref(), TablePlanStep::AddIndex { .. })
        );
        if is_add_index {
            concurrent.push(step.clone());
        } else {
            transactional.push(step.clone());
        }
    }
    (
        DatabasePlan {
            steps: transactional,
        },
        DatabasePlan { steps: concurrent },
    )
}

/// Publishes create-from-scratch DDL straight from a compile-time [`Database`].
pub async fn publish_database<D, B, C>(
    backend: &B,
    connection: &mut C,
) -> Result<(), PublishError<C::Error>>
where
    D: Database,
    B: SchemaBackend,
    C: DdlExecutor,
{
    publish(&DatabaseModel::from_database::<D>(), backend, connection).await
}

/// Reads the live database schema visible to `connection` into the neutral model.
///
/// Backend crates own the catalog queries and type normalization; the management engine only depends
/// on the shared [`SchemaIntrospect`] contract.
pub async fn introspect<C>(connection: &mut C) -> Result<DatabaseModel, C::Error>
where
    C: SchemaIntrospect,
{
    connection.introspect_database().await
}

fn validate_capabilities(
    model: &DatabaseModel,
    capabilities: SchemaCapabilities,
) -> std::io::Result<()> {
    for schema in &model.schemas {
        for table in &schema.tables {
            for column in &table.columns {
                if column.on_update.is_some() && !capabilities.columns.on_update {
                    return unsupported_column(
                        &table.name,
                        &column.name,
                        "an `ON UPDATE` auto-update attribute",
                    );
                }
                // For a backend that *does* carry the attribute, still reject a malformed value here so
                // the preflight does not approve a package the renderer would reject at publish time.
                if capabilities.columns.on_update
                    && let Some(reason) = column.on_update_shape_error()
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("{reason} for column `{}` on `{}`", column.name, table.name),
                    ));
                }
            }
            validate_table_constraint_prefixes(table, &capabilities)?;
            for foreign_key in &table.foreign_keys {
                if foreign_key.match_type.is_some()
                    && !capabilities.constraints.foreign_key_match_type
                {
                    return unsupported_constraint(
                        &table.name,
                        &foreign_key.name,
                        "foreign key match metadata",
                    );
                }
                if foreign_key.deferrability.is_some()
                    && !capabilities.constraints.foreign_key_deferrability
                {
                    return unsupported_constraint(
                        &table.name,
                        &foreign_key.name,
                        "foreign key deferrability metadata",
                    );
                }
                if foreign_key.validation.is_some()
                    && !capabilities.constraints.foreign_key_validation
                {
                    return unsupported_constraint(
                        &table.name,
                        &foreign_key.name,
                        "foreign key validation metadata",
                    );
                }
                if foreign_key.enforcement.is_some()
                    && !capabilities.constraints.foreign_key_enforcement
                {
                    return unsupported_constraint(
                        &table.name,
                        &foreign_key.name,
                        "foreign key enforcement metadata",
                    );
                }
            }
            for check in &table.checks {
                if check.validation.is_some() && !capabilities.constraints.check_validation {
                    return unsupported_constraint(
                        &table.name,
                        &check.name,
                        "check validation metadata",
                    );
                }
                if check.enforcement.is_some() && !capabilities.constraints.check_enforcement {
                    return unsupported_constraint(
                        &table.name,
                        &check.name,
                        "check enforcement metadata",
                    );
                }
            }
            for index in &table.indexes {
                if index.predicate.is_some() && !capabilities.indexes.predicates {
                    return unsupported_index(&table.name, &index.name, "partial index predicates");
                }
                if !index.expressions.is_empty() && !capabilities.indexes.expressions {
                    return unsupported_index(&table.name, &index.name, "index expressions");
                }
                if !index.include_columns.is_empty() && !capabilities.indexes.include_columns {
                    return unsupported_index(&table.name, &index.name, "index include columns");
                }
                if !index.nulls.is_empty() && !capabilities.indexes.null_ordering {
                    return unsupported_index(&table.name, &index.name, "index null ordering");
                }
                if !index.collations.is_empty() && !capabilities.indexes.collations {
                    return unsupported_index(
                        &table.name,
                        &index.name,
                        "index collation overrides",
                    );
                }
                if !index.operator_classes.is_empty() && !capabilities.indexes.operator_classes {
                    return unsupported_index(&table.name, &index.name, "index operator classes");
                }
                if !index.prefix_lengths.is_empty() && !capabilities.indexes.prefix_lengths {
                    return unsupported_index(
                        &table.name,
                        &index.name,
                        "index column prefix lengths",
                    );
                }
            }
        }
    }
    Ok(())
}

fn unsupported_column(table: &str, column: &str, feature: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "backend cannot render and introspect {feature} for column `{column}` on `{table}`"
        ),
    ))
}

/// Validates a table's `UNIQUE`/`PRIMARY KEY` column prefix lengths against the backend-neutral rules:
/// the capability gate (does the backend carry prefixes at all) and the sparse-list shape
/// ([`squealy::prefix_length_shape_error`] — no zero length, each position in range and once). The
/// backend-specific column type/width validation is dispatched separately to
/// [`SchemaBackend::validate_constraint_prefixes`]. Shared by the create preflight
/// ([`validate_capabilities`]) and the incremental render path ([`render_plan_sql`]).
fn validate_table_constraint_prefixes(
    table: &squealy::TableModel,
    capabilities: &SchemaCapabilities,
) -> std::io::Result<()> {
    for constraint in table.primary_key.iter().chain(&table.uniques) {
        if constraint.prefix_lengths.is_empty() {
            continue;
        }
        if !capabilities.constraints.prefix_lengths {
            return unsupported_constraint(
                &table.name,
                &constraint.name,
                "constraint column prefix lengths",
            );
        }
        if let Some(reason) =
            squealy::prefix_length_shape_error(constraint.columns.len(), &constraint.prefix_lengths)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "constraint `{}` {reason} on `{}`",
                    constraint.name, table.name
                ),
            ));
        }
    }
    Ok(())
}

fn unsupported_constraint(table: &str, constraint: &str, feature: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "backend cannot render and introspect {feature} for constraint `{constraint}` on `{table}`"
        ),
    ))
}

fn unsupported_index(table: &str, index: &str, feature: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("backend cannot render and introspect {feature} for index `{index}` on `{table}`"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBackend {
        capabilities: SchemaCapabilities,
    }

    impl SchemaBackend for TestBackend {
        fn capabilities(&self) -> SchemaCapabilities {
            self.capabilities
        }

        fn render_create(
            &self,
            _model: &DatabaseModel,
            writer: &mut impl std::io::Write,
        ) -> std::io::Result<()> {
            writer.write_all(b"-- rendered")
        }
    }

    fn table_with_constraints(foreign_key: ForeignKeyModel, check: CheckModel) -> DatabaseModel {
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
                tables: vec![TableModel {
                    name: "memberships".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys: vec![foreign_key],
                    uniques: vec![],
                    checks: vec![check],
                    indexes: vec![],
                }],
            }],
        }
    }

    fn foreign_key() -> ForeignKeyModel {
        ForeignKeyModel {
            name: "fk_memberships_tenant_id".to_owned(),
            columns: vec!["tenant_id".to_owned()],
            references_schema: None,
            references_table: "tenants".to_owned(),
            references_columns: vec!["id".to_owned()],
            match_type: None,
            deferrability: None,
            validation: None,
            enforcement: None,
            on_delete: None,
            on_update: None,
        }
    }

    fn check_expr(sql: &str) -> squealy::ExprNode {
        squealy_parse::Reader::new(squealy_parse::SqlDialect::Generic)
            .read_check_expression(sql)
            .unwrap()
    }

    fn check() -> CheckModel {
        CheckModel {
            name: "ck_memberships_quota".to_owned(),
            expression: check_expr("quota > 0"),
            validation: None,
            enforcement: None,
        }
    }

    fn table_with_index(index: IndexModel) -> DatabaseModel {
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
                tables: vec![TableModel {
                    name: "memberships".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![index],
                }],
            }],
        }
    }

    fn index() -> IndexModel {
        IndexModel {
            name: "idx_memberships_tenant_id".to_owned(),
            columns: vec!["tenant_id".to_owned()],
            expressions: vec![],
            include_columns: vec![],
            unique: false,
            method: None,
            directions: vec![],
            nulls: vec![],
            collations: vec![],
            operator_classes: vec![],
            prefix_lengths: vec![],
            predicate: None,
        }
    }

    #[test]
    fn canonicalize_model_uses_view_column_hook_for_views_only() {
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "keys".to_owned(),
                    comment: None,
                    columns: vec![ColumnModel {
                        name: "secret".to_owned(),
                        comment: None,
                        ty: SqlType::FixedBytes(16),
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    }],
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                }],
                views: vec![ViewModel {
                    name: "key_view".to_owned(),
                    comment: None,
                    columns: vec![ViewColumnModel {
                        name: "secret".to_owned(),
                        ty: SqlType::FixedBytes(16),
                        nullable: false,
                    }],
                    query: ViewBody::default(),
                }],
            }],
        };

        let canonical = canonicalize_model(&CanonBackend, &model);
        // A table column keeps `FixedBytes` (introspection folds the generated CHECK back into it)...
        assert_eq!(
            canonical.schemas[0].tables[0].columns[0].ty,
            SqlType::FixedBytes(16)
        );
        // ...but a view column canonicalizes to `Bytes` (a view column has no check to fold).
        assert_eq!(canonical.schemas[0].views[0].columns[0].ty, SqlType::Bytes);
    }

    #[test]
    fn canonicalize_model_folds_view_body_result_pins() {
        let view = ViewModel {
            name: "totals".to_owned(),
            comment: None,
            columns: vec![ViewColumnModel {
                name: "s".to_owned(),
                ty: SqlType::I8,
                nullable: false,
            }],
            query: ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "s".to_owned(),
                    internal_alias: None,
                    expr: ExprNode::Aggregate {
                        func: squealy::AggregateFunc::Sum,
                        distinct: false,
                        operand: Box::new(ExprNode::BareColumn {
                            column: "x".to_owned(),
                        }),
                        result: Some(SqlType::I8),
                    },
                }],
                ..Default::default()
            })),
        };
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![],
                views: vec![view],
            }],
        };

        let canonical = canonicalize_model(&CanonBackend, &model);
        let ViewBody::Select(query) = &canonical.schemas[0].views[0].query else {
            panic!("expected a Select body")
        };
        // The backend's `canonical_view_body` folded the aggregate's `I8` result pin to the canonical `I16`.
        let ExprNode::Aggregate { result, .. } = &query.projection[0].expr else {
            panic!("expected an aggregate projection")
        };
        assert_eq!(*result, Some(SqlType::I16));
    }
    /// A backend whose introspection canonical form mirrors MySQL: bare `String` reads back as
    /// `Varchar(255)`, any identity is `AUTO_INCREMENT`, and a plain index has an explicit `BTREE`
    /// method with ASC directions.
    struct CanonBackend;

    impl SchemaIntrospect for CanonBackend {
        type Error = std::io::Error;

        async fn introspect_database(&mut self) -> Result<DatabaseModel, Self::Error> {
            unreachable!("canonicalize_model never introspects")
        }

        fn canonical_sql_type(&self, ty: &SqlType) -> SqlType {
            match ty {
                SqlType::String => SqlType::Varchar(255),
                other => other.clone(),
            }
        }

        fn canonical_view_column_type(&self, ty: &SqlType) -> SqlType {
            match ty {
                SqlType::FixedBytes(_) => SqlType::Bytes,
                other => self.canonical_sql_type(other),
            }
        }

        fn canonical_identity_mode(&self, _mode: &squealy::IdentityMode) -> squealy::IdentityMode {
            squealy::IdentityMode::AutoIncrement
        }

        // A backend whose result-pin cast vocabulary collapses `I8` to a wider canonical `I16`.
        fn canonical_view_body(&self, mut body: squealy::ViewBody) -> squealy::ViewBody {
            body.map_result_pins(&|ty| match ty {
                SqlType::I8 => SqlType::I16,
                other => other.clone(),
            });
            body
        }

        fn default_index_method(&self) -> Option<IndexMethod> {
            Some(IndexMethod::BTree)
        }

        fn canonical_primary_key_name(&self, _name: &str) -> String {
            "PRIMARY".to_owned()
        }

        // A schema-less backend (like SQLite) flattens every namespace and derives constraint names
        // from their columns, since it does not round-trip the declared name.
        fn canonical_schema_name(&self, _name: Option<&str>) -> Option<String> {
            None
        }

        fn has_namespaces(&self) -> bool {
            false
        }

        fn canonical_unique_name(&self, unique: &Constraint) -> String {
            format!("unique:{}", unique.columns.join(","))
        }

        fn canonical_foreign_key_name(&self, foreign_key: &ForeignKeyModel) -> String {
            format!("foreign_key:{}", foreign_key.columns.join(","))
        }

        fn canonical_default(
            &self,
            _ty: &SqlType,
            default: &squealy::DefaultValue,
        ) -> squealy::DefaultValue {
            match default {
                squealy::DefaultValue::Bool(value) => {
                    squealy::DefaultValue::Int(i128::from(*value))
                }
                other => other.clone(),
            }
        }
    }

    #[test]
    fn canonicalize_model_drops_empty_schemas_without_namespaces() {
        // A schema-less backend reports no schema for an empty database, so a desired empty namespace
        // must be dropped (it would otherwise diff as a spurious CreateSchema). A schema with objects is
        // kept. A backend with real schemas keeps the empty schema.
        let model = DatabaseModel {
            schemas: vec![
                SchemaModel {
                    name: Some("empty".to_owned()),
                    views: Vec::new(),
                    tables: Vec::new(),
                },
                SchemaModel {
                    name: Some("app".to_owned()),
                    views: Vec::new(),
                    tables: vec![TableModel {
                        name: "widgets".to_owned(),
                        comment: None,
                        columns: vec![],
                        primary_key: None,
                        foreign_keys: vec![],
                        uniques: vec![],
                        checks: vec![],
                        indexes: vec![],
                    }],
                },
            ],
        };

        let schema_less = canonicalize_model(&CanonBackend, &model);
        assert_eq!(schema_less.schemas.len(), 1);
        assert_eq!(schema_less.schemas[0].tables[0].name, "widgets");

        // A backend with real schemas (DefaultBackend uses the trait defaults) keeps the empty schema.
        let with_schemas = canonicalize_model(&DefaultBackend, &model);
        assert_eq!(with_schemas.schemas.len(), 2);
    }

    #[test]
    fn canonicalize_model_coalesces_flattened_schemas() {
        // Two source schemas both flatten to `None`; their tables must be merged into one schema, not
        // dropped (a `BTreeMap`-keyed diff would otherwise keep only one same-named schema).
        let table = |name: &str| TableModel {
            name: name.to_owned(),
            comment: None,
            columns: vec![],
            primary_key: None,
            foreign_keys: vec![],
            uniques: vec![],
            checks: vec![],
            indexes: vec![],
        };
        let model = DatabaseModel {
            schemas: vec![
                SchemaModel {
                    name: Some("app".to_owned()),
                    views: Vec::new(),
                    tables: vec![table("users")],
                },
                SchemaModel {
                    name: Some("archive".to_owned()),
                    views: Vec::new(),
                    tables: vec![table("logs")],
                },
            ],
        };

        let canonical = canonicalize_model(&CanonBackend, &model);

        assert_eq!(canonical.schemas.len(), 1);
        assert_eq!(canonical.schemas[0].name, None);
        let names: Vec<&str> = canonical.schemas[0]
            .tables
            .iter()
            .map(|table| table.name.as_str())
            .collect();
        assert_eq!(names, vec!["users", "logs"]);
    }

    #[test]
    fn canonicalize_model_normalizes_explicit_no_action() {
        // `NO ACTION` is the referential-action default; introspection reports it as `None`, so an
        // explicit `Some(NoAction)` on the desired side must normalize to `None` (a real action like
        // `Cascade` is preserved). This is backend-neutral, so the trait-default backend applies it.
        let mut foreign_key = foreign_key();
        foreign_key.on_delete = Some(ForeignKeyAction::NoAction);
        foreign_key.on_update = Some(ForeignKeyAction::Cascade);
        let model = table_with_constraints(foreign_key, check());

        let canonical = canonicalize_model(&DefaultBackend, &model);
        let foreign_key = &canonical.schemas[0].tables[0].foreign_keys[0];
        assert_eq!(foreign_key.on_delete, None);
        assert_eq!(foreign_key.on_update, Some(ForeignKeyAction::Cascade));
    }

    #[test]
    fn render_create_rejects_malformed_constraint_prefix_length_shape() {
        // A backend that *advertises* the prefix-length capability must still reject a malformed shape at
        // the preflight, so `check` fails fast rather than the later `script`/`publish` render.
        let mut model = table_with_constraints(foreign_key(), check());
        model.schemas[0].tables[0].uniques = vec![Constraint {
            name: "uq_bad".to_owned(),
            columns: vec!["slug".to_owned()],
            prefix_lengths: vec![IndexPrefixLength {
                position: 0,
                length: 0,
            }],
        }];
        let mut capabilities = SchemaCapabilities::default();
        capabilities.constraints.prefix_lengths = true;

        let error = render_create_sql(&model, &TestBackend { capabilities }).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("zero-length prefix"),
            "unexpected error: {error}"
        );
    }

    /// A one-table model whose sole `UNIQUE` carries a single-column prefix over a text column.
    fn model_with_text_prefix_unique(length: u32) -> DatabaseModel {
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
                tables: vec![TableModel {
                    name: "items".to_owned(),
                    comment: None,
                    columns: vec![ColumnModel {
                        name: "body".to_owned(),
                        comment: None,
                        ty: SqlType::Text,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    }],
                    primary_key: None,
                    foreign_keys: Vec::new(),
                    uniques: vec![Constraint {
                        name: "uq_items".to_owned(),
                        columns: vec!["body".to_owned()],
                        prefix_lengths: vec![IndexPrefixLength {
                            position: 0,
                            length,
                        }],
                    }],
                    checks: Vec::new(),
                    indexes: Vec::new(),
                }],
            }],
        }
    }

    /// A prefix-capable backend whose column-aware `validate_constraint_prefixes` rejects every prefix,
    /// so a test can prove the engine dispatches to it (the type/width rules themselves live in — and are
    /// tested by — each backend crate).
    struct RejectingPrefixBackend;

    impl SchemaBackend for RejectingPrefixBackend {
        fn capabilities(&self) -> SchemaCapabilities {
            let mut capabilities = SchemaCapabilities::default();
            capabilities.constraints.prefix_lengths = true;
            capabilities
        }

        fn validate_constraint_prefixes(
            &self,
            _table: &squealy::TableModel,
        ) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "backend rejects this prefix",
            ))
        }

        fn render_create(
            &self,
            _model: &DatabaseModel,
            writer: &mut impl std::io::Write,
        ) -> std::io::Result<()> {
            writer.write_all(b"-- rendered")
        }
    }

    #[test]
    fn check_create_dispatches_column_aware_prefix_validation_to_the_backend() {
        // The neutral engine defers the type/width validation to the backend; `check_create` and the
        // incremental `render_plan_sql` must both call it.
        let model = model_with_text_prefix_unique(10);
        let error = render_create_sql(&model, &RejectingPrefixBackend).unwrap_err();
        assert!(
            error.to_string().contains("backend rejects this prefix"),
            "{error}"
        );

        let plan = DatabasePlan { steps: Vec::new() };
        let error = render_plan_sql(&plan, &model, &RejectingPrefixBackend).unwrap_err();
        assert!(
            error.to_string().contains("backend rejects this prefix"),
            "{error}"
        );
    }

    #[test]
    fn canonicalize_model_applies_default_hook() {
        let mut model = table_with_index(index());
        model.schemas[0].tables[0].columns.push(ColumnModel {
            name: "active".to_owned(),
            comment: None,
            ty: SqlType::Bool,
            collation: None,
            nullable: false,
            default: Some(squealy::DefaultValue::Bool(true)),
            identity: None,
            generated: None,
            on_update: None,
        });

        let canonical = canonicalize_model(&CanonBackend, &model);
        let column = &canonical.schemas[0].tables[0].columns[0];
        assert_eq!(column.default, Some(squealy::DefaultValue::Int(1)));
    }

    #[test]
    fn canonicalize_model_flattens_schema_and_constraint_names() {
        let mut model = table_with_constraints(foreign_key(), check());
        model.schemas[0].name = Some("app".to_owned());
        model.schemas[0].tables[0].foreign_keys[0].references_schema = Some("app".to_owned());
        model.schemas[0].tables[0].uniques.push(Constraint {
            prefix_lengths: Vec::new(),
            name: "uq_memberships_tenant_id".to_owned(),
            columns: vec!["tenant_id".to_owned()],
        });

        let canonical = canonicalize_model(&CanonBackend, &model);
        let schema = &canonical.schemas[0];

        // The namespace is flattened on the schema and on the cross-schema foreign-key reference.
        assert_eq!(schema.name, None);
        let table = &schema.tables[0];
        assert_eq!(table.foreign_keys[0].references_schema, None);

        // Constraint names are rewritten to the column-derived canonical form.
        assert_eq!(table.uniques[0].name, "unique:tenant_id");
        assert_eq!(table.foreign_keys[0].name, "foreign_key:tenant_id");

        // Two applications are stable (idempotent), so a re-plan against the canonical form is empty.
        assert_eq!(canonicalize_model(&CanonBackend, &canonical), canonical);
        assert!(diff_models(&canonical, &canonical).is_empty());
    }

    #[test]
    fn canonicalize_model_aligns_identity_index_and_type_with_introspection() {
        let mut model = table_with_index(index());
        model.schemas[0].tables[0].columns.push(ColumnModel {
            name: "id".to_owned(),
            comment: None,
            ty: SqlType::String,
            collation: None,
            nullable: false,
            default: None,
            identity: Some(squealy::IdentityModel {
                mode: squealy::IdentityMode::ByDefault,
            }),
            generated: None,
            on_update: None,
        });
        model.schemas[0].tables[0].primary_key = Some(Constraint {
            prefix_lengths: Vec::new(),
            name: "pk_memberships".to_owned(),
            columns: vec!["id".to_owned()],
        });

        let canonical = canonicalize_model(&CanonBackend, &model);
        let table = &canonical.schemas[0].tables[0];

        let column = &table.columns[0];
        assert_eq!(column.ty, SqlType::Varchar(255));
        assert_eq!(
            column.identity.as_ref().unwrap().mode,
            squealy::IdentityMode::AutoIncrement
        );

        assert_eq!(table.primary_key.as_ref().unwrap().name, "PRIMARY");

        let index = &table.indexes[0];
        assert_eq!(index.method, Some(IndexMethod::BTree));
        assert_eq!(index.directions, vec![IndexDirection::Asc]);

        // The canonicalized desired model now matches what introspection reads back, so a diff after
        // a clean publish is empty instead of a never-settling AlterColumn/AlterIndex.
        assert!(diff_models(&canonical, &canonical).is_empty());
    }

    /// A backend using the trait defaults: it leaves default index metadata unset, so introspection
    /// reports `method: None` / `directions: []`. Canonicalization must not invent a `BTree` method or
    /// ASC directions for it, which would itself create a never-settling `AlterIndex`.
    struct DefaultBackend;

    impl SchemaIntrospect for DefaultBackend {
        type Error = std::io::Error;

        async fn introspect_database(&mut self) -> Result<DatabaseModel, Self::Error> {
            unreachable!("canonicalize_model never introspects")
        }
    }

    #[test]
    fn canonicalize_model_leaves_index_defaults_for_backends_that_do_not_fill_them() {
        let model = table_with_index(index());

        let canonical = canonicalize_model(&DefaultBackend, &model);
        let canonical_index = &canonical.schemas[0].tables[0].indexes[0];

        assert_eq!(canonical_index.method, None);
        assert!(canonical_index.directions.is_empty());
        // The desired index is unchanged, so it still matches what such a backend introspects.
        assert!(diff_models(&canonical, &model).is_empty());
    }

    #[test]
    fn canonicalize_model_trims_trailing_ascending_directions_for_no_default_method() {
        // A backend that leaves index metadata unset (e.g. SQLite) omits the default (ASC) sort order,
        // so trailing `Asc` directions are trimmed to match what it introspects.
        let canonicalized = |directions: Vec<IndexDirection>| {
            let mut index = index();
            index.directions = directions;
            canonicalize_model(&DefaultBackend, &table_with_index(index)).schemas[0].tables[0]
                .indexes[0]
                .directions
                .clone()
        };
        // All-ascending collapses to empty.
        assert!(canonicalized(vec![IndexDirection::Asc, IndexDirection::Asc]).is_empty());
        // A non-default prefix keeps the prefix but drops the trailing implicit `Asc`.
        assert_eq!(
            canonicalized(vec![IndexDirection::Desc, IndexDirection::Asc]),
            vec![IndexDirection::Desc]
        );
        // An `Asc` before a non-default `Desc` is kept (only trailing `Asc` is implicit).
        assert_eq!(
            canonicalized(vec![IndexDirection::Asc, IndexDirection::Desc]),
            vec![IndexDirection::Asc, IndexDirection::Desc]
        );
    }

    #[test]
    fn render_create_rejects_unsupported_constraint_capabilities() {
        let mut foreign_key = foreign_key();
        foreign_key.validation = Some(ConstraintValidation::NotValidated);
        let mut check = check();
        check.enforcement = Some(ConstraintEnforcement::NotEnforced);
        let model = table_with_constraints(foreign_key, check);

        let error = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities::default(),
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error
                .to_string()
                .contains("foreign key validation metadata"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn render_create_rejects_unsupported_constraint_prefix_length_capability() {
        // A backend that does not carry constraint column prefix lengths must reject a `UNIQUE`/
        // `PRIMARY KEY` carrying one at the capability preflight, before it is rendered.
        let mut model = table_with_constraints(foreign_key(), check());
        model.schemas[0].tables[0].uniques = vec![Constraint {
            name: "uq_memberships_slug".to_owned(),
            columns: vec!["slug".to_owned()],
            prefix_lengths: vec![IndexPrefixLength {
                position: 0,
                length: 8,
            }],
        }];

        let error = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities::default(),
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error
                .to_string()
                .contains("constraint column prefix lengths"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn render_create_rejects_unsupported_foreign_key_shape_capabilities() {
        let mut foreign_key = foreign_key();
        foreign_key.match_type = Some(ForeignKeyMatch::Full);
        foreign_key.deferrability = Some(ConstraintDeferrability::InitiallyDeferred);
        let model = table_with_constraints(foreign_key, check());

        let error = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities::default(),
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("foreign key match metadata"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn render_create_rejects_unsupported_index_capabilities() {
        let mut index = index();
        index.predicate = Some(Box::new(squealy::ExprNode::IsNull {
            negated: true,
            operand: Box::new(squealy::ExprNode::BareColumn {
                column: "tenant_id".to_owned(),
            }),
        }));
        index.expressions = vec![squealy::ExprNode::ScalarFn {
            func: squealy::ScalarFunc::Lower,
            args: vec![squealy::ExprNode::BareColumn {
                column: "name".to_owned(),
            }],
        }];
        index.include_columns = vec!["created_at".to_owned()];
        index.nulls = vec![IndexNullsOrder::Last];
        index.collations = vec![IndexCollation {
            position: 0,
            name: "C".to_owned(),
        }];
        index.operator_classes = vec![IndexOperatorClass {
            position: 0,
            name: "text_pattern_ops".to_owned(),
        }];
        index.prefix_lengths = vec![IndexPrefixLength {
            position: 0,
            length: 10,
        }];
        let model = table_with_index(index);

        let error = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities::default(),
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("partial index predicates"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn render_create_rejects_unsupported_column_on_update_capability() {
        // A MySQL-authored package carrying `on_update` must fail the capability preflight for a backend
        // that does not report the column capability, so `check`/`render_create` do not approve an
        // unrenderable package (git-bug 7f4504d).
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
                tables: vec![TableModel {
                    name: "events".to_owned(),
                    comment: None,
                    columns: vec![ColumnModel {
                        name: "updated_at".to_owned(),
                        comment: None,
                        ty: SqlType::Timestamp {
                            tz: true,
                            precision: None,
                        },
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: Some(Box::new(squealy::ExprNode::Now)),
                    }],
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                }],
            }],
        };

        let error = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities::default(),
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("ON UPDATE"),
            "unexpected error: {error}"
        );

        // A backend that reports the capability accepts it.
        let on_update_capable = TestBackend {
            capabilities: SchemaCapabilities {
                columns: ColumnCapabilities { on_update: true },
                ..SchemaCapabilities::default()
            },
        };
        render_create_sql(&model, &on_update_capable)
            .expect("a backend reporting the column capability renders");

        // Even a capable backend rejects a malformed value (here a non-temporal column) in preflight, so
        // `check` does not approve a package the renderer would reject at publish time.
        let mut malformed = model;
        malformed.schemas[0].tables[0].columns[0].ty = SqlType::I32;
        let error = check_create(&malformed, &on_update_capable).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn render_create_allows_reported_constraint_capabilities() {
        let mut foreign_key = foreign_key();
        foreign_key.validation = Some(ConstraintValidation::NotValidated);
        let mut check = check();
        check.enforcement = Some(ConstraintEnforcement::NotEnforced);
        let model = table_with_constraints(foreign_key, check);

        let sql = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities {
                    columns: ColumnCapabilities::default(),
                    constraints: ConstraintCapabilities {
                        foreign_key_match_type: false,
                        foreign_key_deferrability: false,
                        foreign_key_validation: true,
                        foreign_key_enforcement: false,
                        check_validation: false,
                        check_enforcement: true,
                        prefix_lengths: false,
                    },
                    indexes: IndexCapabilities::default(),
                },
            },
        )
        .expect("reported capabilities should allow rendering");

        assert_eq!(sql, "-- rendered");
    }

    #[test]
    fn render_create_allows_reported_index_capabilities() {
        let mut index = index();
        index.predicate = Some(Box::new(squealy::ExprNode::IsNull {
            negated: true,
            operand: Box::new(squealy::ExprNode::BareColumn {
                column: "tenant_id".to_owned(),
            }),
        }));
        index.expressions = vec![squealy::ExprNode::ScalarFn {
            func: squealy::ScalarFunc::Lower,
            args: vec![squealy::ExprNode::BareColumn {
                column: "name".to_owned(),
            }],
        }];
        index.include_columns = vec!["created_at".to_owned()];
        index.nulls = vec![IndexNullsOrder::Last];
        index.collations = vec![IndexCollation {
            position: 0,
            name: "C".to_owned(),
        }];
        index.operator_classes = vec![IndexOperatorClass {
            position: 0,
            name: "text_pattern_ops".to_owned(),
        }];
        index.prefix_lengths = vec![IndexPrefixLength {
            position: 0,
            length: 10,
        }];
        let model = table_with_index(index);

        let sql = render_create_sql(
            &model,
            &TestBackend {
                capabilities: SchemaCapabilities {
                    columns: ColumnCapabilities::default(),
                    constraints: ConstraintCapabilities::default(),
                    indexes: IndexCapabilities {
                        predicates: true,
                        expressions: true,
                        include_columns: true,
                        null_ordering: true,
                        collations: true,
                        operator_classes: true,
                        prefix_lengths: true,
                    },
                },
            },
        )
        .expect("reported capabilities should allow rendering");

        assert_eq!(sql, "-- rendered");
    }
}
