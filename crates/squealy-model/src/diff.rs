//! Name-based diffing for owned schema models.
//!
//! This module compares a desired [`DatabaseModel`] with an actual [`DatabaseModel`] and reports the
//! structural changes needed to make the actual model match the desired model. It does not infer
//! renames or render SQL; those are later planning/rendering steps.

use std::collections::{BTreeMap, BTreeSet};

use squealy::{
    CheckModel, ColumnModel, Constraint, DatabaseModel, DomainModel, EnumModel, ForeignKeyModel,
    IndexModel, SchemaModel, SequenceModel, TableModel, ViewColumnModel, ViewModel,
};

/// The structured diff from an actual database model to a desired database model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DatabaseDiff {
    pub changes: Vec<DatabaseDiffChange>,
}

impl DatabaseDiff {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn classified_changes(&self) -> Vec<ClassifiedDatabaseDiffChange> {
        self.changes
            .iter()
            .map(|change| ClassifiedDatabaseDiffChange {
                risk: change.risk(),
                change: change.clone(),
            })
            .collect()
    }
}

/// A diff change with conservative deployment-risk classification.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassifiedDatabaseDiffChange {
    pub risk: ChangeRisk,
    pub change: DatabaseDiffChange,
}

/// Conservative risk classification for a schema change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeRisk {
    Safe,
    Destructive,
    Ambiguous,
}

/// Policy for deciding whether a diff is acceptable to apply automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffPolicy {
    pub allow_destructive: bool,
    pub allow_ambiguous: bool,
}

impl DiffPolicy {
    pub const BLOCK_RISKY: Self = Self {
        allow_destructive: false,
        allow_ambiguous: false,
    };

    pub const ALLOW_ALL: Self = Self {
        allow_destructive: true,
        allow_ambiguous: true,
    };

    pub fn allows(self, risk: ChangeRisk) -> bool {
        match risk {
            ChangeRisk::Safe => true,
            ChangeRisk::Destructive => self.allow_destructive,
            ChangeRisk::Ambiguous => self.allow_ambiguous,
        }
    }
}

impl Default for DiffPolicy {
    fn default() -> Self {
        Self::BLOCK_RISKY
    }
}

/// Error returned when a diff contains changes blocked by a [`DiffPolicy`].
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("diff contains {} blocked change(s)", blocked.len())]
pub struct DiffPolicyError {
    pub blocked: Vec<ClassifiedDatabaseDiffChange>,
}

/// Error returned when two schema objects that PostgreSQL cannot give the same name — a sequence versus
/// a table/view/index, or an enum versus a table/view/sequence — share a `(schema, name)` across the
/// actual and desired models. Relations (tables, views, sequences, indexes) share one per-schema
/// `pg_class` namespace, and a table/view/sequence additionally owns a composite `pg_type` alongside
/// enums. Correctly ordering such a *swap* plus its arbitrary dependents is deferred, so it is rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{} is used as two schema objects that PostgreSQL cannot give the same name — a sequence and a \
     table/view/index, or an enum and a table/view/sequence — across the current and desired schemas. \
     Relations share one `pg_class` namespace and a table/view/sequence owns a composite type alongside \
     enums in `pg_type`. If you are replacing one with another, split it into separate migrations that \
     fully drop and apply one before introducing the other.",
    qualified_name(schema, name)
)]
pub struct EnumRelationCollisionError {
    pub schema: Option<String>,
    pub name: String,
}

/// A schema object kind that occupies PostgreSQL's per-schema relation/type namespaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ObjectKind {
    Table,
    View,
    Sequence,
    Enum,
    Domain,
    Index,
}

/// Whether two *different* object kinds sharing a `(schema, name)` is a swap this diff cannot order and
/// so rejects. The matrix is exactly what PostgreSQL 17 enforces for the sequence/enum pairs this guard
/// owns (verified empirically):
/// - a **sequence** is a `pg_class` relation *and* owns an associated type, so it clashes with every
///   table, view, index, and with an enum or domain;
/// - an **enum** and a **domain** live in `pg_type`, so each clashes with a table's/view's/sequence's
///   associated type and with the other — but *not* with an index, which has none.
///
/// A plain table↔view (or table↔index) name clash is left to the existing name-based diff, which keys
/// those separately; this guard only covers the un-orderable sequence/enum/domain swaps.
fn kinds_collide(a: ObjectKind, b: ObjectKind) -> bool {
    use ObjectKind::*;
    if a == b {
        return false;
    }
    // A sequence is a `pg_class` relation that also owns a type, so it clashes with every other kind.
    if a == Sequence || b == Sequence {
        return true;
    }
    // An enum or domain lives only in `pg_type`, so it clashes with any object that owns a composite
    // type of the same name (a table, view, or the other enum/domain) — but not an index, which has none.
    let type_like = |k| matches!(k, Enum | Domain);
    let composite_bearer = |k| matches!(k, Table | View | Enum | Domain);
    if type_like(a) || type_like(b) {
        return composite_bearer(a) && composite_bearer(b);
    }
    // Both are plain `pg_class` relations (table/view/index); that clash is left to the name-based diff.
    false
}

/// Records that `(schema, name)` is claimed by object `kind`, returning an error if any *different* kind
/// that [collides](kinds_collide) with it already claimed the same name. The same kind claiming it twice
/// — e.g. a table present in both the desired and actual model — is fine, as is a non-colliding different
/// kind (a table and a same-named view, an enum and a same-named index). All kinds seen for a name are
/// retained, so a later colliding claim is caught regardless of the order they arrive in.
fn claim_object_name(
    claims: &mut BTreeMap<(Option<String>, String), BTreeSet<ObjectKind>>,
    schema: &Option<String>,
    name: &str,
    kind: ObjectKind,
) -> Result<(), EnumRelationCollisionError> {
    let kinds = claims.entry((schema.clone(), name.to_owned())).or_default();
    if kinds.iter().any(|existing| kinds_collide(*existing, kind)) {
        return Err(EnumRelationCollisionError {
            schema: schema.clone(),
            name: name.to_owned(),
        });
    }
    kinds.insert(kind);
    Ok(())
}

fn qualified_name(schema: &Option<String>, name: &str) -> String {
    match schema {
        Some(schema) => format!("{schema}.{name}"),
        None => name.to_string(),
    }
}

/// Rejects a migration in which a single `(schema, name)` is claimed by two schema objects PostgreSQL
/// keeps in overlapping namespaces (see [`kinds_collide`] for the exact matrix) — across the desired and
/// actual models, or within one. Correctly ordering such a swap plus its arbitrary dependents is
/// deferred, so detecting the collision here keeps `diff_models` from emitting an un-applyable plan.
pub fn reject_enum_relation_name_collision(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
) -> Result<(), EnumRelationCollisionError> {
    let mut claims: BTreeMap<(Option<String>, String), BTreeSet<ObjectKind>> = BTreeMap::new();
    for model in [desired, actual] {
        for schema in &model.schemas {
            for table in &schema.tables {
                claim_object_name(&mut claims, &schema.name, &table.name, ObjectKind::Table)?;
                // A PRIMARY KEY / UNIQUE constraint and an explicit index each own a `pg_class` index of
                // that name, which a same-named sequence would collide with.
                for index in &table.indexes {
                    claim_object_name(&mut claims, &schema.name, &index.name, ObjectKind::Index)?;
                }
                if let Some(primary_key) = &table.primary_key {
                    claim_object_name(
                        &mut claims,
                        &schema.name,
                        &primary_key.name,
                        ObjectKind::Index,
                    )?;
                }
                for unique in &table.uniques {
                    claim_object_name(&mut claims, &schema.name, &unique.name, ObjectKind::Index)?;
                }
            }
            for view in &schema.views {
                claim_object_name(&mut claims, &schema.name, &view.name, ObjectKind::View)?;
            }
            for sequence in &schema.sequences {
                claim_object_name(
                    &mut claims,
                    &schema.name,
                    &sequence.name,
                    ObjectKind::Sequence,
                )?;
            }
            for enum_type in &schema.enums {
                claim_object_name(&mut claims, &schema.name, &enum_type.name, ObjectKind::Enum)?;
            }
            for domain in &schema.domains {
                claim_object_name(&mut claims, &schema.name, &domain.name, ObjectKind::Domain)?;
            }
        }
    }
    Ok(())
}

/// Rejects a precomputed diff that touches one `(schema, name)` as more than one kind of schema object
/// (table, view, sequence, or enum). [`reject_enum_relation_name_collision`] catches the collision from
/// a model pair; this catches it from a diff a caller assembled and handed to
/// [`plan_diff`](crate::plan_diff) directly (e.g. a same-name relation↔enum or relation↔sequence swap,
/// whose create/drop the flattener would otherwise emit in an order PostgreSQL rejects over the shared
/// `pg_class`/`pg_type` namespace).
pub fn reject_enum_relation_collision_in_diff(
    diff: &DatabaseDiff,
) -> Result<(), EnumRelationCollisionError> {
    let mut claims: BTreeMap<(Option<String>, String), BTreeSet<ObjectKind>> = BTreeMap::new();
    for change in &diff.changes {
        match change {
            DatabaseDiffChange::CreateTable { schema, table }
            | DatabaseDiffChange::DropTable { schema, table } => {
                claim_object_name(&mut claims, schema, &table.name, ObjectKind::Table)?;
                for index in &table.indexes {
                    claim_object_name(&mut claims, schema, &index.name, ObjectKind::Index)?;
                }
                if let Some(primary_key) = &table.primary_key {
                    claim_object_name(&mut claims, schema, &primary_key.name, ObjectKind::Index)?;
                }
                for unique in &table.uniques {
                    claim_object_name(&mut claims, schema, &unique.name, ObjectKind::Index)?;
                }
            }
            DatabaseDiffChange::AlterTable {
                schema,
                table,
                changes,
            } => {
                claim_object_name(&mut claims, schema, table, ObjectKind::Table)?;
                for change in changes {
                    match change {
                        TableDiffChange::AddIndex { index }
                        | TableDiffChange::DropIndex { index } => {
                            claim_object_name(&mut claims, schema, &index.name, ObjectKind::Index)?;
                        }
                        TableDiffChange::AlterIndex { before, after } => {
                            claim_object_name(
                                &mut claims,
                                schema,
                                &before.name,
                                ObjectKind::Index,
                            )?;
                            claim_object_name(&mut claims, schema, &after.name, ObjectKind::Index)?;
                        }
                        TableDiffChange::AddPrimaryKey { constraint }
                        | TableDiffChange::DropPrimaryKey { constraint }
                        | TableDiffChange::AddUnique { constraint }
                        | TableDiffChange::DropUnique { constraint } => {
                            claim_object_name(
                                &mut claims,
                                schema,
                                &constraint.name,
                                ObjectKind::Index,
                            )?;
                        }
                        TableDiffChange::AlterPrimaryKey { before, after }
                        | TableDiffChange::AlterUnique { before, after } => {
                            claim_object_name(
                                &mut claims,
                                schema,
                                &before.name,
                                ObjectKind::Index,
                            )?;
                            claim_object_name(&mut claims, schema, &after.name, ObjectKind::Index)?;
                        }
                        _ => {}
                    }
                }
            }
            DatabaseDiffChange::CreateView { schema, view }
            | DatabaseDiffChange::DropView { schema, view } => {
                claim_object_name(&mut claims, schema, &view.name, ObjectKind::View)?;
            }
            DatabaseDiffChange::CreateEnum { schema, enum_type }
            | DatabaseDiffChange::DropEnum { schema, enum_type } => {
                claim_object_name(&mut claims, schema, &enum_type.name, ObjectKind::Enum)?;
            }
            DatabaseDiffChange::AlterEnum { schema, after, .. } => {
                claim_object_name(&mut claims, schema, &after.name, ObjectKind::Enum)?;
            }
            DatabaseDiffChange::CreateSequence { schema, sequence }
            | DatabaseDiffChange::DropSequence { schema, sequence } => {
                claim_object_name(&mut claims, schema, &sequence.name, ObjectKind::Sequence)?;
            }
            DatabaseDiffChange::AlterSequence { schema, after, .. } => {
                claim_object_name(&mut claims, schema, &after.name, ObjectKind::Sequence)?;
            }
            DatabaseDiffChange::SetSequenceOwner { schema, name, .. } => {
                claim_object_name(&mut claims, schema, name, ObjectKind::Sequence)?;
            }
            DatabaseDiffChange::CreateDomain { schema, domain }
            | DatabaseDiffChange::DropDomain { schema, domain } => {
                claim_object_name(&mut claims, schema, &domain.name, ObjectKind::Domain)?;
            }
            DatabaseDiffChange::AlterDomain { schema, after, .. } => {
                claim_object_name(&mut claims, schema, &after.name, ObjectKind::Domain)?;
            }
            DatabaseDiffChange::CreateSchema { .. } | DatabaseDiffChange::DropSchema { .. } => {}
        }
    }
    Ok(())
}

/// Checks whether `diff` is allowed by `policy`.
pub fn check_diff_policy(diff: &DatabaseDiff, policy: DiffPolicy) -> Result<(), DiffPolicyError> {
    let blocked = diff
        .classified_changes()
        .into_iter()
        .filter(|change| !policy.allows(change.risk))
        .collect::<Vec<_>>();

    if blocked.is_empty() {
        Ok(())
    } else {
        Err(DiffPolicyError { blocked })
    }
}

/// A database-level change.
#[derive(Clone, Debug, PartialEq)]
pub enum DatabaseDiffChange {
    CreateSchema {
        schema: Option<String>,
    },
    DropSchema {
        schema: Option<String>,
    },
    CreateTable {
        schema: Option<String>,
        table: TableModel,
    },
    DropTable {
        schema: Option<String>,
        table: TableModel,
    },
    AlterTable {
        schema: Option<String>,
        table: String,
        changes: Vec<TableDiffChange>,
    },
    /// Create-or-replace a view. A view-body change is expressed as a single `CreateView` (rendered
    /// `CREATE OR REPLACE VIEW`); a change that alters the column set is a `DropView` + `CreateView`.
    CreateView {
        schema: Option<String>,
        view: ViewModel,
    },
    DropView {
        schema: Option<String>,
        view: ViewModel,
    },
    /// Create a new enum type. Rendered before any table so a column can reference it.
    CreateEnum {
        schema: Option<String>,
        enum_type: EnumModel,
    },
    /// Drop an enum type. Rendered after any table that referenced it is gone.
    DropEnum {
        schema: Option<String>,
        enum_type: EnumModel,
    },
    /// Change an enum's labels. Appending labels is a safe in-place `ALTER TYPE ... ADD VALUE`; any other
    /// change (removing or reordering labels) requires recreating the type and rewriting its columns —
    /// destructive. `additive` distinguishes the two for risk classification.
    AlterEnum {
        schema: Option<String>,
        before: EnumModel,
        after: EnumModel,
        additive: bool,
    },
    /// Create a new sequence. Rendered before any table (a column default may `nextval` it) but without
    /// its `OWNED BY` clause — that column may not exist yet, so ownership is a separate post-table
    /// [`SetSequenceOwner`](DatabaseDiffChange::SetSequenceOwner).
    CreateSequence {
        schema: Option<String>,
        sequence: SequenceModel,
    },
    /// Drop a sequence. Rendered after any table whose column default referenced it is gone.
    DropSequence {
        schema: Option<String>,
        sequence: SequenceModel,
    },
    /// Change a sequence's attributes (type/start/increment/bounds/cache/cycle) in place with
    /// `ALTER SEQUENCE`. Ownership is not part of this — see
    /// [`SetSequenceOwner`](DatabaseDiffChange::SetSequenceOwner).
    AlterSequence {
        schema: Option<String>,
        before: SequenceModel,
        after: SequenceModel,
    },
    /// Set (or clear) a sequence's owning column via `ALTER SEQUENCE ... OWNED BY`. Rendered after tables
    /// so the owning column exists, distinct from the sequence's create/attribute changes.
    SetSequenceOwner {
        schema: Option<String>,
        name: String,
        owned_by: Option<squealy::SequenceOwnedBy>,
    },
    /// Create a new domain type. Rendered before any table so a column can be of it.
    CreateDomain {
        schema: Option<String>,
        domain: DomainModel,
    },
    /// Drop a domain type. Rendered after any table that referenced it is gone.
    DropDomain {
        schema: Option<String>,
        domain: DomainModel,
    },
    /// Change a domain's `NOT NULL`, `DEFAULT`, or `CHECK` constraints in place with `ALTER DOMAIN`.
    AlterDomain {
        schema: Option<String>,
        before: DomainModel,
        after: DomainModel,
    },
}

impl DatabaseDiffChange {
    pub fn risk(&self) -> ChangeRisk {
        match self {
            DatabaseDiffChange::CreateSchema { .. }
            | DatabaseDiffChange::CreateTable { .. }
            // Create-or-replace of a view loses no data and can be re-run.
            | DatabaseDiffChange::CreateView { .. }
            | DatabaseDiffChange::CreateEnum { .. }
            // Creating a sequence, altering its attributes, or (re)assigning its owner loses no data.
            | DatabaseDiffChange::CreateSequence { .. }
            | DatabaseDiffChange::AlterSequence { .. }
            | DatabaseDiffChange::SetSequenceOwner { .. }
            // Creating a domain, or altering its constraints, loses no data (a tightened CHECK can fail
            // to apply, but is not itself destructive).
            | DatabaseDiffChange::CreateDomain { .. }
            | DatabaseDiffChange::AlterDomain { .. } => ChangeRisk::Safe,
            DatabaseDiffChange::DropSchema { .. }
            | DatabaseDiffChange::DropTable { .. }
            | DatabaseDiffChange::DropView { .. }
            | DatabaseDiffChange::DropEnum { .. }
            | DatabaseDiffChange::DropSequence { .. }
            | DatabaseDiffChange::DropDomain { .. } => ChangeRisk::Destructive,
            // Appending enum labels is safe; recreating the type (remove/reorder) rewrites columns.
            DatabaseDiffChange::AlterEnum { additive, .. } => {
                if *additive {
                    ChangeRisk::Safe
                } else {
                    ChangeRisk::Destructive
                }
            }
            DatabaseDiffChange::AlterTable { changes, .. } => classify_table_changes(changes),
        }
    }
}

/// A table-level change.
#[derive(Clone, Debug, PartialEq)]
pub enum TableDiffChange {
    SetTableComment {
        before: Option<String>,
        after: Option<String>,
    },
    AddColumn {
        column: ColumnModel,
    },
    DropColumn {
        column: ColumnModel,
    },
    AlterColumn {
        before: ColumnModel,
        after: ColumnModel,
    },
    AddPrimaryKey {
        constraint: Constraint,
    },
    DropPrimaryKey {
        constraint: Constraint,
    },
    AlterPrimaryKey {
        before: Constraint,
        after: Constraint,
    },
    AddUnique {
        constraint: Constraint,
    },
    DropUnique {
        constraint: Constraint,
    },
    AlterUnique {
        before: Constraint,
        after: Constraint,
    },
    AddForeignKey {
        foreign_key: ForeignKeyModel,
    },
    DropForeignKey {
        foreign_key: ForeignKeyModel,
    },
    AlterForeignKey {
        before: ForeignKeyModel,
        after: ForeignKeyModel,
    },
    AddCheck {
        check: CheckModel,
    },
    DropCheck {
        check: CheckModel,
    },
    AlterCheck {
        before: CheckModel,
        after: CheckModel,
    },
    AddIndex {
        index: IndexModel,
    },
    DropIndex {
        index: IndexModel,
    },
    AlterIndex {
        before: IndexModel,
        after: IndexModel,
    },
}

impl TableDiffChange {
    pub fn risk(&self) -> ChangeRisk {
        match self {
            TableDiffChange::SetTableComment { .. }
            | TableDiffChange::AddPrimaryKey { .. }
            | TableDiffChange::AddUnique { .. }
            | TableDiffChange::AddForeignKey { .. }
            | TableDiffChange::AddCheck { .. }
            | TableDiffChange::AddIndex { .. } => ChangeRisk::Safe,
            TableDiffChange::DropColumn { .. }
            | TableDiffChange::DropPrimaryKey { .. }
            | TableDiffChange::DropUnique { .. }
            | TableDiffChange::DropForeignKey { .. }
            | TableDiffChange::DropCheck { .. }
            | TableDiffChange::DropIndex { .. } => ChangeRisk::Destructive,
            TableDiffChange::AddColumn { column } => {
                if column.nullable || column.default.is_some() || column.identity.is_some() {
                    ChangeRisk::Safe
                } else {
                    ChangeRisk::Ambiguous
                }
            }
            TableDiffChange::AlterColumn { .. }
            | TableDiffChange::AlterPrimaryKey { .. }
            | TableDiffChange::AlterUnique { .. }
            | TableDiffChange::AlterForeignKey { .. }
            | TableDiffChange::AlterCheck { .. }
            | TableDiffChange::AlterIndex { .. } => ChangeRisk::Ambiguous,
        }
    }
}

fn classify_table_changes(changes: &[TableDiffChange]) -> ChangeRisk {
    let mut risk = ChangeRisk::Safe;
    for change in changes {
        match change.risk() {
            ChangeRisk::Destructive => return ChangeRisk::Destructive,
            ChangeRisk::Ambiguous => risk = ChangeRisk::Ambiguous,
            ChangeRisk::Safe => {}
        }
    }
    risk
}

/// Compares `desired` with `actual`.
///
/// Returned changes are phrased as actions required to make `actual` match `desired`. Identity is
/// name-based: schema name, table name within schema, column name within table, and constraint/index
/// name within table.
pub fn diff_models(desired: &DatabaseModel, actual: &DatabaseModel) -> DatabaseDiff {
    let desired_schemas = keyed_schemas(&desired.schemas);
    let actual_schemas = keyed_schemas(&actual.schemas);
    let mut changes = Vec::new();

    // Views are diffed across the whole model (not one schema at a time) so a view can depend on a view
    // in another schema. Drops run before every table/schema change (a view may select from a table
    // being dropped) and creates run after all of them (a view may select from a table or schema being
    // added); the two phases bracket the per-schema table work below.
    let (view_drops, view_creates) = diff_views_global(desired, actual);

    // Enum types bracket the table work the *opposite* way to views: a `CREATE TYPE` (and any label
    // change) must precede the tables whose columns reference it, and a `DROP TYPE` must follow the
    // tables that referenced it. But a new enum still needs its schema to exist, and a dropped schema
    // must outlive its enum — so `CREATE SCHEMA` is hoisted *before* enum creates and `DROP SCHEMA`
    // *after* enum drops. Overall order: create schemas → create/alter enums → drop views → per-schema
    // table work → create views → drop enums → drop schemas.
    //
    // (An enum name colliding with a same-named table/view being replaced in the same migration is
    // rejected earlier by `reject_enum_relation_name_collision`, so no cross-object-kind ordering is
    // needed here — PostgreSQL auto-creates a composite type per relation, and correctly ordering that
    // swap plus its arbitrary dependents is deferred as a separate feature.)
    let (enum_creates, enum_drops) = diff_enums_global(desired, actual);

    // Sequences bracket the table work like enums: a `CREATE SEQUENCE` (and any attribute change) must
    // precede a table whose column default `nextval`s it, and a `DROP SEQUENCE` must follow the table
    // whose default referenced it. A sequence's `OWNED BY <table>.<column>`, however, needs the owning
    // column to exist, so it is deferred to the post-table phase alongside drops.
    let (sequence_pre_table, sequence_post_table) = diff_sequences_global(desired, actual);

    // Domains bracket the table work exactly like enums (both are `pg_type`): a `CREATE DOMAIN` (and any
    // constraint change) precedes the tables whose columns are of the domain, and a `DROP DOMAIN` follows
    // them.
    let (domain_creates, domain_drops) = diff_domains_global(desired, actual);

    for schema_key in sorted_keys(&desired_schemas, &actual_schemas) {
        if let (Some(desired_schema), None) = (
            desired_schemas.get(&schema_key),
            actual_schemas.get(&schema_key),
        ) {
            changes.push(DatabaseDiffChange::CreateSchema {
                schema: desired_schema.name.clone(),
            });
        }
    }

    changes.extend(enum_creates);
    // Sequences precede domains: a domain's `DEFAULT` may `nextval` a sequence created in the same
    // migration, but a sequence never references a domain.
    changes.extend(sequence_pre_table);
    changes.extend(domain_creates);
    changes.extend(view_drops);

    for schema_key in sorted_keys(&desired_schemas, &actual_schemas) {
        match (
            desired_schemas.get(&schema_key),
            actual_schemas.get(&schema_key),
        ) {
            (Some(desired_schema), None) => {
                // The schema was created in the hoisted phase above; create its tables here.
                for table in &desired_schema.tables {
                    changes.push(DatabaseDiffChange::CreateTable {
                        schema: desired_schema.name.clone(),
                        table: table.clone(),
                    });
                }
            }
            (None, Some(actual_schema)) => {
                for table in &actual_schema.tables {
                    changes.push(DatabaseDiffChange::DropTable {
                        schema: actual_schema.name.clone(),
                        table: table.clone(),
                    });
                }
            }
            (Some(desired_schema), Some(actual_schema)) => {
                diff_schema_tables(desired_schema, actual_schema, &mut changes);
            }
            (None, None) => {}
        }
    }

    changes.extend(view_creates);
    // Domains drop before enums: a domain based on an enum (carried as a `Raw` base) depends on it, so
    // the enum's `DROP TYPE` must follow the domain's `DROP DOMAIN`.
    changes.extend(domain_drops);
    changes.extend(enum_drops);
    changes.extend(sequence_post_table);

    for schema_key in sorted_keys(&desired_schemas, &actual_schemas) {
        if let (None, Some(actual_schema)) = (
            desired_schemas.get(&schema_key),
            actual_schemas.get(&schema_key),
        ) {
            changes.push(DatabaseDiffChange::DropSchema {
                schema: actual_schema.name.clone(),
            });
        }
    }

    DatabaseDiff { changes }
}

/// Diffs enum types across the whole model, returning `(creates_and_alters, drops)`. Identity is
/// `(schema, name)`. A label change is an `AlterEnum`, flagged `additive` when the actual labels are a
/// prefix of the desired ones (an append-only change PostgreSQL can apply in place with
/// `ALTER TYPE ... ADD VALUE`); any other change (removal, reorder, mid-insert) is non-additive and
/// requires recreating the type.
fn diff_enums_global(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
) -> (Vec<DatabaseDiffChange>, Vec<DatabaseDiffChange>) {
    let desired_enums = keyed_enums(desired);
    let actual_enums = keyed_enums(actual);
    let mut creates = Vec::new();
    let mut drops = Vec::new();

    for (key, desired_enum) in &desired_enums {
        match actual_enums.get(key) {
            None => creates.push(DatabaseDiffChange::CreateEnum {
                schema: key.0.clone(),
                enum_type: (*desired_enum).clone(),
            }),
            Some(actual_enum) if actual_enum.labels != desired_enum.labels => {
                // A change is additive when every existing label is preserved in order — i.e. the actual
                // labels are an ordered *subsequence* of the desired ones (PostgreSQL can insert a value
                // anywhere with `ALTER TYPE ... ADD VALUE ... BEFORE/AFTER`, so a mid-list insertion is
                // still non-destructive). A removal or reorder breaks the subsequence and is destructive.
                let additive = is_ordered_subsequence(&actual_enum.labels, &desired_enum.labels);
                creates.push(DatabaseDiffChange::AlterEnum {
                    schema: key.0.clone(),
                    before: (*actual_enum).clone(),
                    after: (*desired_enum).clone(),
                    additive,
                });
            }
            Some(_) => {}
        }
    }
    for (key, actual_enum) in &actual_enums {
        if !desired_enums.contains_key(key) {
            drops.push(DatabaseDiffChange::DropEnum {
                schema: key.0.clone(),
                enum_type: (*actual_enum).clone(),
            });
        }
    }
    (creates, drops)
}

/// Whether `subset` appears as an ordered subsequence of `sequence` (every element of `subset` occurs
/// in `sequence`, in the same relative order).
fn is_ordered_subsequence(subset: &[String], sequence: &[String]) -> bool {
    let mut iter = sequence.iter();
    subset
        .iter()
        .all(|item| iter.any(|candidate| candidate == item))
}

/// Keys every enum in the model by `(schema, name)`.
fn keyed_enums(model: &DatabaseModel) -> BTreeMap<(Option<String>, String), &EnumModel> {
    let mut enums = BTreeMap::new();
    for schema in &model.schemas {
        for enum_type in &schema.enums {
            enums.insert((schema.name.clone(), enum_type.name.clone()), enum_type);
        }
    }
    enums
}

/// Diffs domain types across the whole model, returning `(creates_and_alters, drops)`. Identity is
/// `(schema, name)`. A domain that differs is an `AlterDomain` (the renderer applies granular
/// `ALTER DOMAIN` statements for its `NOT NULL` / `DEFAULT` / `CHECK` changes, or refuses a base-type
/// change it cannot perform in place). Creates/alters are rendered before tables (a column may be of the
/// domain), drops after.
fn diff_domains_global(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
) -> (Vec<DatabaseDiffChange>, Vec<DatabaseDiffChange>) {
    let desired_domains = keyed_domains(desired);
    let actual_domains = keyed_domains(actual);
    let mut creates = Vec::new();
    let mut drops = Vec::new();

    for (key, desired_domain) in &desired_domains {
        match actual_domains.get(key) {
            None => creates.push(DatabaseDiffChange::CreateDomain {
                schema: key.0.clone(),
                domain: (*desired_domain).clone(),
            }),
            Some(actual_domain) if domains_differ(actual_domain, desired_domain) => {
                creates.push(DatabaseDiffChange::AlterDomain {
                    schema: key.0.clone(),
                    before: (*actual_domain).clone(),
                    after: (*desired_domain).clone(),
                });
            }
            Some(_) => {}
        }
    }
    for (key, actual_domain) in &actual_domains {
        if !desired_domains.contains_key(key) {
            drops.push(DatabaseDiffChange::DropDomain {
                schema: key.0.clone(),
                domain: (*actual_domain).clone(),
            });
        }
    }
    (creates, drops)
}

/// Whether two domains differ, comparing their `CHECK` constraints as an unordered set (constraint
/// declaration order carries no meaning, and introspection returns them name-sorted). Canonicalization
/// already sorts them on the plan path; this keeps a direct `diff_models` over two packages from
/// reporting a spurious `AlterDomain` when only the check order differs.
fn domains_differ(a: &DomainModel, b: &DomainModel) -> bool {
    let sorted_checks = |domain: &DomainModel| {
        let mut checks = domain.checks.clone();
        checks.sort_by(|x, y| x.name.cmp(&y.name));
        checks
    };
    a.name != b.name
        || a.base_type != b.base_type
        || a.not_null != b.not_null
        || a.default != b.default
        || sorted_checks(a) != sorted_checks(b)
}

/// Keys every domain in the model by `(schema, name)`.
fn keyed_domains(model: &DatabaseModel) -> BTreeMap<(Option<String>, String), &DomainModel> {
    let mut domains = BTreeMap::new();
    for schema in &model.schemas {
        for domain in &schema.domains {
            domains.insert((schema.name.clone(), domain.name.clone()), domain);
        }
    }
    domains
}

/// Diffs sequences across the whole model, returning `(pre_table, post_table)` change lists. Identity is
/// `(schema, name)`.
///
/// The **pre-table** list runs before all table work — `CreateSequence` (without owner, so a column
/// default can `nextval` a new sequence), `AlterSequence` (attribute change), and a *detach*
/// (`SetSequenceOwner` → `NONE`) for any sequence whose *current* owner is going away (the sequence is
/// being dropped, or its owner is being changed/removed). Detaching before the table phase is what keeps
/// PostgreSQL from cascade-dropping an owned sequence when its owning table/column is dropped.
///
/// The **post-table** list runs after all table work — an *attach* (`SetSequenceOwner` → the desired
/// owner) once the new owning column exists, and `DropSequence` for a removed sequence (now safely
/// detached, so its owner's drop did not already cascade it away).
fn diff_sequences_global(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
) -> (Vec<DatabaseDiffChange>, Vec<DatabaseDiffChange>) {
    let desired_sequences = keyed_sequences(desired);
    let actual_sequences = keyed_sequences(actual);
    let mut pre_table = Vec::new();
    let mut post_table = Vec::new();

    // Detach any existing sequence whose current owner is about to disappear, before the table phase.
    let detach = |pre_table: &mut Vec<DatabaseDiffChange>, key: &(Option<String>, String)| {
        pre_table.push(DatabaseDiffChange::SetSequenceOwner {
            schema: key.0.clone(),
            name: key.1.clone(),
            owned_by: None,
        });
    };

    for (key, desired_sequence) in &desired_sequences {
        match actual_sequences.get(key) {
            None => {
                pre_table.push(DatabaseDiffChange::CreateSequence {
                    schema: key.0.clone(),
                    sequence: sequence_without_owner(desired_sequence),
                });
                if desired_sequence.owned_by.is_some() {
                    post_table.push(DatabaseDiffChange::SetSequenceOwner {
                        schema: key.0.clone(),
                        name: desired_sequence.name.clone(),
                        owned_by: desired_sequence.owned_by.clone(),
                    });
                }
            }
            Some(actual_sequence) => {
                if attributes_differ(actual_sequence, desired_sequence) {
                    pre_table.push(DatabaseDiffChange::AlterSequence {
                        schema: key.0.clone(),
                        before: sequence_without_owner(actual_sequence),
                        after: sequence_without_owner(desired_sequence),
                    });
                }
                if actual_sequence.owned_by != desired_sequence.owned_by {
                    // The old owner is going away: release it before the table phase (so a drop of that
                    // owner cannot cascade the sequence), then re-attach to the desired owner afterward.
                    if actual_sequence.owned_by.is_some() {
                        detach(&mut pre_table, key);
                    }
                    if desired_sequence.owned_by.is_some() {
                        post_table.push(DatabaseDiffChange::SetSequenceOwner {
                            schema: key.0.clone(),
                            name: desired_sequence.name.clone(),
                            owned_by: desired_sequence.owned_by.clone(),
                        });
                    }
                }
            }
        }
    }
    for (key, actual_sequence) in &actual_sequences {
        if !desired_sequences.contains_key(key) {
            // Detach first so dropping the owner does not cascade the sequence out from under the
            // explicit `DropSequence` below.
            if actual_sequence.owned_by.is_some() {
                detach(&mut pre_table, key);
            }
            post_table.push(DatabaseDiffChange::DropSequence {
                schema: key.0.clone(),
                sequence: sequence_without_owner(actual_sequence),
            });
        }
    }
    (pre_table, post_table)
}

/// Whether two sequences differ in any attribute other than their owning column (which is diffed
/// separately, in the post-table phase).
fn attributes_differ(a: &SequenceModel, b: &SequenceModel) -> bool {
    sequence_without_owner(a) != sequence_without_owner(b)
}

/// A copy of `sequence` with its owner cleared, so attribute comparison/rendering ignores ownership.
fn sequence_without_owner(sequence: &SequenceModel) -> SequenceModel {
    SequenceModel {
        owned_by: None,
        ..sequence.clone()
    }
}

/// Keys every sequence in the model by `(schema, name)`.
fn keyed_sequences(model: &DatabaseModel) -> BTreeMap<(Option<String>, String), &SequenceModel> {
    let mut sequences = BTreeMap::new();
    for schema in &model.schemas {
        for sequence in &schema.sequences {
            sequences.insert((schema.name.clone(), sequence.name.clone()), sequence);
        }
    }
    sequences
}

fn diff_schema_tables(
    desired: &SchemaModel,
    actual: &SchemaModel,
    changes: &mut Vec<DatabaseDiffChange>,
) {
    let desired_tables = keyed_tables(&desired.tables);
    let actual_tables = keyed_tables(&actual.tables);

    for table_name in sorted_keys(&desired_tables, &actual_tables) {
        match (
            desired_tables.get(&table_name),
            actual_tables.get(&table_name),
        ) {
            (Some(desired_table), None) => changes.push(DatabaseDiffChange::CreateTable {
                schema: desired.name.clone(),
                table: (*desired_table).clone(),
            }),
            (None, Some(actual_table)) => changes.push(DatabaseDiffChange::DropTable {
                schema: actual.name.clone(),
                table: (*actual_table).clone(),
            }),
            (Some(desired_table), Some(actual_table)) => {
                let table_changes = diff_table(desired_table, actual_table);
                if !table_changes.is_empty() {
                    changes.push(DatabaseDiffChange::AlterTable {
                        schema: desired.name.clone(),
                        table: table_name,
                        changes: table_changes,
                    });
                }
            }
            (None, None) => {}
        }
    }
}

/// Diffs every view in the model, returning `(drops, creates)` so the caller can emit drops before
/// table changes and creates after. A view present in both that differs becomes a create-or-replace
/// (`CreateView`); when its column set changes it also needs a preceding `DropView`, since
/// `CREATE OR REPLACE VIEW` cannot rename or retype columns.
/// A view's global identity across the model: its schema (`None` = the unqualified schema) and name.
type ViewKey = (Option<String>, String);

/// Every view in the model, keyed by `(schema, name)`.
fn all_views(model: &DatabaseModel) -> BTreeMap<ViewKey, &ViewModel> {
    let mut views = BTreeMap::new();
    for schema in &model.schemas {
        for view in &schema.views {
            views.insert((schema.name.clone(), view.name.clone()), view);
        }
    }
    views
}

/// The `(schema, name)` a `CreateView`/`DropView` change targets.
fn view_change_key(change: &DatabaseDiffChange) -> ViewKey {
    match change {
        DatabaseDiffChange::CreateView { schema, view }
        | DatabaseDiffChange::DropView { schema, view } => (schema.clone(), view.name.clone()),
        _ => (None, String::new()),
    }
}

fn diff_views_global(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
) -> (Vec<DatabaseDiffChange>, Vec<DatabaseDiffChange>) {
    let desired_views = all_views(desired);
    let actual_views = all_views(actual);

    let mut drops = Vec::new();
    let mut creates = Vec::new();
    for key in sorted_keys(&desired_views, &actual_views) {
        let schema = key.0.clone();
        match (desired_views.get(&key), actual_views.get(&key)) {
            (Some(desired_view), None) => creates.push(DatabaseDiffChange::CreateView {
                schema,
                view: (*desired_view).clone(),
            }),
            (None, Some(actual_view)) => drops.push(DatabaseDiffChange::DropView {
                schema,
                view: (*actual_view).clone(),
            }),
            (Some(desired_view), Some(actual_view)) => {
                // A live-introspected view carries no structural body (it can't be reconstructed from
                // the stored SQL), so its body cannot be compared against the desired one. Rather than
                // treat a same-shape view as unchanged (which would never re-apply a changed `SELECT`
                // body), conservatively re-apply the desired definition as a `CREATE OR REPLACE VIEW`
                // every run — idempotent and non-destructive. The replace can't change the column set,
                // so a column change still drops first. Per-column nullability is unreliable when
                // introspected (PostgreSQL's `pg_attribute.attnotnull` is usually false for view
                // outputs) and a view's DDL carries no per-column NOT NULL, so the column comparison
                // ignores it. Models that carry a body (e.g. from a package) are compared in full.
                //
                // An introspected view whose body could not be reconstructed has an empty `SELECT` body
                // (only its name, columns, and dependencies are recovered) — the "body unknown" marker.
                if actual_view.query.is_empty() {
                    if view_columns_differ_ignoring_nullability(
                        &desired_view.columns,
                        &actual_view.columns,
                    ) {
                        drops.push(DatabaseDiffChange::DropView {
                            schema: schema.clone(),
                            view: (*actual_view).clone(),
                        });
                    }
                    creates.push(DatabaseDiffChange::CreateView {
                        schema,
                        view: (*desired_view).clone(),
                    });
                } else if desired_view != actual_view {
                    if desired_view.columns != actual_view.columns {
                        drops.push(DatabaseDiffChange::DropView {
                            schema: schema.clone(),
                            view: (*actual_view).clone(),
                        });
                    }
                    creates.push(DatabaseDiffChange::CreateView {
                        schema,
                        view: (*desired_view).clone(),
                    });
                }
            }
            (None, None) => {}
        }
    }

    // A view cannot be dropped while another live view still depends on it, so dropping/recreating one
    // (for a column-set change or a removal) forces its transitive dependents — in any schema — to be
    // dropped first and recreated after. Expand the drop/recreate set to that closure over the live
    // dependency graph; the sort below puts each dependent ahead of the view it selects from.
    let mut dropped: BTreeSet<ViewKey> = drops.iter().map(view_change_key).collect();
    // Reverse edges: which live views select from each view, keyed by the dependency's effective
    // `(schema, name)` (an unqualified source resolves to its own view's schema).
    let mut dependents_of: BTreeMap<ViewKey, Vec<ViewKey>> = BTreeMap::new();
    for (key, view) in &actual_views {
        for source in view.referenced_sources() {
            let dependency = (
                source.schema.clone().or_else(|| key.0.clone()),
                source.name.clone(),
            );
            dependents_of
                .entry(dependency)
                .or_default()
                .push(key.clone());
        }
    }
    let mut worklist: Vec<ViewKey> = dropped.iter().cloned().collect();
    while let Some(key) = worklist.pop() {
        let Some(dependents) = dependents_of.get(&key) else {
            continue;
        };
        for dependent in dependents.clone() {
            if !dropped.insert(dependent.clone()) {
                continue;
            }
            worklist.push(dependent.clone());
            if let Some(actual_view) = actual_views.get(&dependent) {
                drops.push(DatabaseDiffChange::DropView {
                    schema: dependent.0.clone(),
                    view: (*actual_view).clone(),
                });
            }
            // Recreate the dependent from the desired model if it still exists and a recreate is not
            // already queued (e.g. from the conservative `CREATE OR REPLACE` of an introspected view).
            if let Some(desired_view) = desired_views.get(&dependent)
                && !creates
                    .iter()
                    .any(|change| view_change_key(change) == dependent)
            {
                creates.push(DatabaseDiffChange::CreateView {
                    schema: dependent.0.clone(),
                    view: (*desired_view).clone(),
                });
            }
        }
    }

    // Order creates dependencies-first (a view after every other view it selects from) and drops
    // dependents-first, spanning schemas, so a view-on-view never references a sibling that does not
    // exist yet (create) or has already been removed (drop).
    let desired_order = view_dependency_order(&desired_views);
    let actual_order = view_dependency_order(&actual_views);
    creates.sort_by_key(|change| dependency_rank(&desired_order, &view_change_key(change)));
    drops.sort_by_key(|change| {
        std::cmp::Reverse(dependency_rank(&actual_order, &view_change_key(change)))
    });
    (drops, creates)
}

fn view_columns_differ_ignoring_nullability(
    desired: &[ViewColumnModel],
    actual: &[ViewColumnModel],
) -> bool {
    desired.len() != actual.len()
        || desired
            .iter()
            .zip(actual)
            .any(|(desired, actual)| desired.name != actual.name || desired.ty != actual.ty)
}

/// The view's name from a `CreateView`/`DropView` change.
fn dependency_rank(order: &[ViewKey], key: &ViewKey) -> usize {
    order
        .iter()
        .position(|candidate| candidate == key)
        .unwrap_or(usize::MAX)
}

/// Returns every view's key in dependency order — a view appears after every other view it selects
/// from (resolving an unqualified source to its own schema). A depth-first post-order; reference
/// cycles (which SQL rejects) fall back to map order.
fn view_dependency_order(views: &BTreeMap<ViewKey, &ViewModel>) -> Vec<ViewKey> {
    fn visit(
        index: usize,
        keys: &[ViewKey],
        views: &BTreeMap<ViewKey, &ViewModel>,
        visited: &mut [bool],
        order: &mut Vec<ViewKey>,
    ) {
        if visited[index] {
            return;
        }
        visited[index] = true;
        let (schema, _) = &keys[index];
        for source in views[&keys[index]].referenced_sources() {
            let dependency = (
                source.schema.clone().or_else(|| schema.clone()),
                source.name.clone(),
            );
            if let Some(dependency_index) = keys.iter().position(|key| *key == dependency)
                && dependency_index != index
            {
                visit(dependency_index, keys, views, visited, order);
            }
        }
        order.push(keys[index].clone());
    }

    let keys: Vec<ViewKey> = views.keys().cloned().collect();
    let mut order = Vec::with_capacity(keys.len());
    let mut visited = vec![false; keys.len()];
    for index in 0..keys.len() {
        visit(index, &keys, views, &mut visited, &mut order);
    }
    order
}

pub(crate) fn diff_table(desired: &TableModel, actual: &TableModel) -> Vec<TableDiffChange> {
    let mut changes = Vec::new();

    if desired.comment != actual.comment {
        changes.push(TableDiffChange::SetTableComment {
            before: actual.comment.clone(),
            after: desired.comment.clone(),
        });
    }

    diff_named_vec(
        &desired.columns,
        &actual.columns,
        |column| column.name.clone(),
        |column| TableDiffChange::AddColumn {
            column: column.clone(),
        },
        |column| TableDiffChange::DropColumn {
            column: column.clone(),
        },
        |before, after| TableDiffChange::AlterColumn {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    // A constraint's prefix lengths are keyed by column position and render order-independently, but
    // `Constraint` derives order-sensitive `PartialEq`. Normalize the order before comparing so two
    // equivalent constraints — from a hand-built model diffed directly, a package, or introspection —
    // do not diff as a spurious `Alter{PrimaryKey,Unique}`. This is the single behavioral point where
    // prefix order matters, so normalization lives here rather than at every model entry point.
    let sort_prefixes = |constraint: &Constraint| {
        let mut constraint = constraint.clone();
        constraint
            .prefix_lengths
            .sort_by_key(|prefix| prefix.position);
        constraint
    };
    let desired_primary_key = desired.primary_key.as_ref().map(&sort_prefixes);
    let actual_primary_key = actual.primary_key.as_ref().map(&sort_prefixes);
    diff_primary_key(&desired_primary_key, &actual_primary_key, &mut changes);

    let desired_uniques: Vec<Constraint> = desired.uniques.iter().map(&sort_prefixes).collect();
    let actual_uniques: Vec<Constraint> = actual.uniques.iter().map(&sort_prefixes).collect();
    diff_named_vec(
        &desired_uniques,
        &actual_uniques,
        |constraint| constraint.name.clone(),
        |constraint| TableDiffChange::AddUnique {
            constraint: constraint.clone(),
        },
        |constraint| TableDiffChange::DropUnique {
            constraint: constraint.clone(),
        },
        |before, after| TableDiffChange::AlterUnique {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    diff_named_vec(
        &desired.foreign_keys,
        &actual.foreign_keys,
        |foreign_key| foreign_key.name.clone(),
        |foreign_key| TableDiffChange::AddForeignKey {
            foreign_key: foreign_key.clone(),
        },
        |foreign_key| TableDiffChange::DropForeignKey {
            foreign_key: foreign_key.clone(),
        },
        |before, after| TableDiffChange::AlterForeignKey {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    diff_named_vec(
        &desired.checks,
        &actual.checks,
        |check| check.name.clone(),
        |check| TableDiffChange::AddCheck {
            check: check.clone(),
        },
        |check| TableDiffChange::DropCheck {
            check: check.clone(),
        },
        |before, after| TableDiffChange::AlterCheck {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    diff_named_vec(
        &desired.indexes,
        &actual.indexes,
        |index| index.name.clone(),
        |index| TableDiffChange::AddIndex {
            index: index.clone(),
        },
        |index| TableDiffChange::DropIndex {
            index: index.clone(),
        },
        |before, after| TableDiffChange::AlterIndex {
            before: before.clone(),
            after: after.clone(),
        },
        &mut changes,
    );

    changes
}

fn diff_primary_key(
    desired: &Option<Constraint>,
    actual: &Option<Constraint>,
    changes: &mut Vec<TableDiffChange>,
) {
    match (desired, actual) {
        (Some(desired), None) => changes.push(TableDiffChange::AddPrimaryKey {
            constraint: desired.clone(),
        }),
        (None, Some(actual)) => changes.push(TableDiffChange::DropPrimaryKey {
            constraint: actual.clone(),
        }),
        (Some(desired), Some(actual)) if desired != actual => {
            changes.push(TableDiffChange::AlterPrimaryKey {
                before: actual.clone(),
                after: desired.clone(),
            });
        }
        _ => {}
    }
}

fn diff_named_vec<T, Key, Add, Drop, Alter>(
    desired: &[T],
    actual: &[T],
    key: Key,
    add: Add,
    drop: Drop,
    alter: Alter,
    changes: &mut Vec<TableDiffChange>,
) where
    T: PartialEq,
    Key: Fn(&T) -> String,
    Add: Fn(&T) -> TableDiffChange,
    Drop: Fn(&T) -> TableDiffChange,
    Alter: Fn(&T, &T) -> TableDiffChange,
{
    let desired = keyed(desired, &key);
    let actual = keyed(actual, &key);

    for item_key in sorted_keys(&desired, &actual) {
        match (desired.get(&item_key), actual.get(&item_key)) {
            (Some(desired_item), None) => changes.push(add(desired_item)),
            (None, Some(actual_item)) => changes.push(drop(actual_item)),
            (Some(desired_item), Some(actual_item)) if *desired_item != *actual_item => {
                changes.push(alter(actual_item, desired_item));
            }
            _ => {}
        }
    }
}

fn keyed_schemas(schemas: &[SchemaModel]) -> BTreeMap<Option<String>, &SchemaModel> {
    schemas
        .iter()
        .map(|schema| (schema.name.clone(), schema))
        .collect()
}

fn keyed_tables(tables: &[TableModel]) -> BTreeMap<String, &TableModel> {
    tables
        .iter()
        .map(|table| (table.name.clone(), table))
        .collect()
}

fn keyed<'a, T, Key>(items: &'a [T], key: &Key) -> BTreeMap<String, &'a T>
where
    Key: Fn(&T) -> String,
{
    items.iter().map(|item| (key(item), item)).collect()
}

fn sorted_keys<K, V>(left: &BTreeMap<K, V>, right: &BTreeMap<K, V>) -> Vec<K>
where
    K: Clone + Ord,
{
    left.keys()
        .chain(right.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
