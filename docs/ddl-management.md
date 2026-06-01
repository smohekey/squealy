# DDL Management — Design

Status: **draft / discussion** · Branch: `worktree-ddl-management`

## Context

Squealy already has a strong *declarative* source of truth: the `#[derive(Table/Schema/Database)]`
types are the schema, exposed as a fully walkable compile-time model
([`Database`](../crates/squealy/src/database.rs) → [`Schema`](../crates/squealy/src/schema.rs) →
[`Table`](../crates/squealy/src/table.rs) → [`Column`](../crates/squealy/src/column.rs) /
[`Index`](../crates/squealy/src/index.rs) / [`ForeignKey`](../crates/squealy/src/foreign_key.rs)).
What it lacks is *management*: turning that model into real database state and keeping the two in
sync over time.

The model for this work is Microsoft's **DACFx / SqlPackage**: a declarative deployment system where
the engine lives in libraries and the CLI is a thin wrapper. We adopt that shape, with one important
change of polarity — see below.

## Philosophy & polarity (vs DACFx)

| DACFx | Squealy |
|---|---|
| `.sqlproj` (source of truth) | the Rust **database crate** (`#[derive]` types) |
| **dacpac** (zip + `model.xml`) | **db model package** (zip + KDL) — an *optional, derived* deploy artifact |
| Build (sqlproj → dacpac) | walk the in-memory `Database` model (no build artifact required) |
| Extract (DB → dacpac) | introspect live DB → model *(future sprint)* |
| Script / DeployReport | dry-run: render plan without applying |
| Publish (dacpac → DB) | apply plan to a connection, with configurable policy |
| Schema Compare | diff two models (crate, package, or live DB) *(future sprint)* |
| Publish options (`BlockOnPossibleDataLoss`, …) | `PublishOptions` policy struct |

**Key difference:** a dacpac is the *primary* artifact. Here the **Rust crate is primary** and the
package is a *derived export*. Consequences:

- **Git is the schema history.** Git already versions the Rust source, and the `#[derive]` types are
  the canonical, diff-friendly schema. `git log` on the schema module *is* the history. No artifact
  to canonicalize, no source/artifact drift.
- **Declarative-first, hybrid-capable.** Because comparisons are model-vs-model, a future
  "generate a reviewable upgrade script from rev A → rev B" (the hybrid flow) needs no rework.
- **Deploy without a toolchain** is the package's job: hand a `.szp` to an environment that has no
  Rust/cargo.

## Architecture

The spine is a neutral, owned, serializable model that decouples *where the schema comes from* from
*what we do with it*.

```
        sources                      DatabaseModel               operations
  ┌──────────────────────┐                                ┌──────────────────────────┐
  │ Rust crate           │── walk ──┐                 ┌──→│ render create-from-scratch│
  │  (D: Database)        │          │                 │   ├──────────────────────────┤
  ├──────────────────────┤          ├─→ DatabaseModel ┼──→│ export → package (KDL/zip)│
  │ package file (.szp)   │── load ──┤   (owned,neutral)│   ├──────────────────────────┤
  ├──────────────────────┤          │                 └──→│ diff(desired, actual)*    │
  │ live DB (introspect)* │── read ──┘                      └──────────────────────────┘
  └──────────────────────┘                                   * = future sprint
```

### The owned model (in core `squealy`)

Runtime/serializable mirror of the compile-time trait model. "Schema" here means **namespace**,
consistent with `#[derive(Schema)]`.

> **Placement (revised during implementation):** the model types *and* the `SchemaBackend` trait live
> in **core `squealy`**, not `squealy-model`. `SchemaBackend::render_create` must reference
> `DatabaseModel`, and backends must implement it without depending on the management engine —
> otherwise `squealy-postgresql → squealy-model → squealy` plus `squealy → squealy-model` would cycle.
> Putting both in core (next to `Backend`/`write_table`) keeps backends depending only on core. The
> `squealy-model` *engine* crate owns the operations (package, script/publish, CLI) and the heavier
> deps (KDL, zip).
>
> Column type and default are owned mirrors **`SqlType`** / **`DefaultValue`** (not the compile-time
> `ColumnType`/`ColumnDefault`, which borrow `'static` strings and so can't be rebuilt from a package
> or introspection). `SqlType` is where the neutral type vocabulary grows structurally later.

Constraints are **hoisted to the table level as named lists** (not hung off columns as in the query
traits). Columns keep only per-column facts; PK/unique/FK/check/index are table-level and named. This
matches `ALTER … ADD CONSTRAINT`, how catalogs report constraints during introspection, and admits
composite keys even though today's derive only emits single-column ones (the *shape* won't change
when multi-column lands).

```rust
pub struct DatabaseModel { pub schemas: Vec<SchemaModel> }
pub struct SchemaModel   { pub name: Option<String>, pub tables: Vec<TableModel> }

pub struct TableModel {
    pub name: String,
    pub columns: Vec<ColumnModel>,        // type, nullable, default, identity, generated
    pub primary_key: Option<Constraint>,  // named, possibly composite
    pub foreign_keys: Vec<ForeignKey>,    // named
    pub uniques: Vec<Constraint>,         // named
    pub checks: Vec<Check>,               // named
    pub indexes: Vec<Index>,              // named (already derived deterministically)
}

pub struct ColumnModel {
    pub name: String,
    pub ty: ColumnType,                   // reuse the existing logical enum (see Type system)
    pub nullable: bool,
    pub default: Option<ColumnDefault>,   // reuse the existing enum
    pub auto_increment: bool,
    pub generated: bool,
}
```

Built three ways: `From<&dyn Database>` (walk the derives, flattening per-column constraint data into
the table-level lists), deserialize from a package, or introspect a live DB (future). Every operation
takes `DatabaseModel`(s), so "operate on the crate" vs "on a package" vs "against live DB" is just
*which source you plugged in*.

> Crate name is `squealy-model` (not `squealy-schema`): it is the database-model engine, and "schema"
> is reserved for the namespace concept.

### Constraint naming = diff identity

Every constraint gets a stable, deterministic name; that name doubles as the **identity the future
diff uses to match constraints across versions**. Conventions:

`pk_<table>` · `fk_<table>_<cols>` · `uq_<table>_<cols>` · `ck_<table>_<cols-or-ordinal>` ·
`idx_<table>_<cols>`

- Deterministic by default, with an **optional explicit name override** (needed to adopt an existing
  DB whose constraints are named differently).
- **Name-based identity:** a rename reads as drop+add until explicit *rename hints* are added (deferred
  to the diff sprint) — standard for declarative tools.

### Type system

The logical [`ColumnType`](../crates/squealy/src/column.rs) is intentionally thin (`I8..U128`,
`F32/F64`, `String`, `Bool`, `Raw(&str)`); anything else collapses to `Raw`, which is opaque to the
model. That is fine for **sprint 1**, where types come only from `HasColumnType` (primitives) and
`db_type` overrides (→ `Raw`) — no introspection yet.

Decisions:
- **Do not expand `ColumnType` this sprint.**
- **Commit to growing it structurally** when introspection lands (e.g. `Varchar { len }`,
  `Decimal { precision, scale }`, `Timestamp { tz }`, `Uuid`, `Json`/`Jsonb`, `Bytes`, `Array(_)`),
  keeping `Raw` for the long tail.
- **Design the package type encoding to hold structure from day one** so growth isn't a breaking
  format change — e.g. `column "x" type="varchar" length=64`, never `type="varchar(64)"` as one
  opaque string.

### Crate / binary layout

```
crates/
  squealy              (exists)  metadata traits + query AST
                                  + owned model (DatabaseModel, SqlType, DefaultValue, …)
                                  + DatabaseModel::from_database::<D>() walker
                                  + SchemaBackend trait
  squealy-postgresql   (exists)  backend: query + SchemaBackend (create render; introspect later)
  squealy-model        (NEW lib) engine over the core model: KDL/zip .sqz package,
                                  render_create_sql/script/publish, ddl_main!/cli.
                                  Heavier deps (kdl, zip) isolated here.
  squealy-cli          (later)   stub-compiling global CLI (sqlpackage-like UX)
```

Backend-specific DDL rendering, introspection, and ALTERs sit behind a **`SchemaBackend` trait in
core `squealy`** (sibling to `Backend`, where backends already implement capabilities). This avoids a
dependency inversion: `squealy-postgresql` implements `SchemaBackend`, `squealy-model` consumes it,
and no backend crate has to depend on the management engine. The new whole-DB, ordered,
ALTER-aware renderer **supersedes `Backend::write_table`**, which is migrated under `SchemaBackend`
and retired.

### Front-end

The engine is a set of generic functions over `D: Database` + a backend. Two ways to get a binary
that calls them:

1. **Per-project bin / macro (sprint 1).** Ship a library entry
   `squealy_model::cli::<D, B: SchemaBackend>(args)` *and* a `ddl_main!` macro that expands to a
   `main` calling it — so a hand-written bin and the macro are the same one-liner ("both" for free):

   ```rust
   squealy::ddl_main!(AppDatabase, squealy_postgresql::Postgres);
   // ⇒ fn main() { squealy_model::cli::<AppDatabase, Postgres>(std::env::args()) }
   ```

   Then `cargo run --bin squealy-ddl -- publish …`. Zero orchestration; the in-memory model is right
   there.
2. **Stub-compiling global CLI (later).** A `squealy` binary reads config
   (`[package.metadata.squealy]`: which crate, which `Database` type, which backend — Rust has no
   runtime reflection, so the type must be *named*), generates a temp stub crate that `use`s it,
   `cargo build`s and runs it. Same engine underneath. UX nicety; deferred.

## Package format — KDL in a zip (`.sqz`)

A deploy artifact, *not* the committed source of truth. Direct git-diff prettiness is a nice-to-have,
not a requirement (git tracks the Rust source).

**Container:** zip (the dacpac approach). Single distributable file with room to grow
(pre/post scripts, multiple namespaces) later.

```
package.sqz (zip)
├── manifest.kdl     // metadata, read without parsing the whole model
└── model.kdl        // the DatabaseModel
```

**Grammar:** positional args = the column list; name + everything else are properties; repeated
constructs (columns, constraints) are child nodes. Composite keys fall out as multiple positional
args. `nullable` is emitted explicitly (semantically critical); purely-default flags are omitted for
canonical brevity. Types are structured-ready (`type="varchar" length=64`, never `"varchar(64)"`).

**`manifest.kdl`:**

```kdl
manifest {
  format-version 1
  squealy-version "0.1.0"
  created-at "2026-06-01T00:00:00Z"   // RFC3339
  neutral #true                       // backend-neutral model (else target="postgresql")
  model-hash "blake3:..."             // integrity + fast equality
}
```

**`model.kdl` (illustrative, KDL 2.0):**

```kdl
database {
  schema "public" {
    table "users" {
      column "id"     type="i32"    auto-increment=#true
      column "name"   type="string"
      column "org_id" type="i32"
      primary-key "id"     name="pk_users"
      unique      "name"   name="uq_users_name"
      foreign-key "org_id" name="fk_users_org_id" references="public.organizations.id" on-delete="cascade"
      index       "name"   name="idx_users_name"
    }
    table "organizations" {
      column "id"   type="i32" auto-increment=#true
      column "name" type="string"
      primary-key "id" name="pk_organizations"
    }
  }
}
```

Notes:
- **Backend-neutral:** columns use logical `ColumnType`s; per-backend SQL is rendered at publish time.
- **Deterministic:** stable ordering of schemas/tables/columns and zeroed zip timestamps, so a given
  model serializes byte-reproducibly. Enables comparing release packages without rebuilding each
  Rust revision (the future historical-compare path).
- **Likely crates:** `kdl` (build/parse `KdlDocument` for full control of canonical output — preferred
  over a serde bridge for deterministic emit) and `zip`.

## Sprint 1 — scope (just "bootstrap from scratch")

Build the spine and a real, executable create-from-scratch path, plus the package round-trip.

**In scope**

1. **`DatabaseModel` + `From<&dyn Database>`** — the owned neutral model and the crate walker.
2. **KDL/zip package** — `export` (model → `.szp`) and `import` (`.szp` → model), with `manifest.kdl`
   and deterministic output.
3. **Ordered create-from-scratch DDL** — topo-sort tables by FK deps; emit `CREATE SCHEMA IF NOT
   EXISTS`, `CREATE TABLE`, indexes, and **FKs as separate `ALTER TABLE … ADD CONSTRAINT`** (so
   cycles and ordering stop mattering — generalizes today's inline `REFERENCES` in
   [`write_table`](../crates/squealy-postgresql/src/sql.rs)).
4. **`script` (dry-run) and `publish` (apply)** — generic over `D: Database` + backend; publish runs
   against a connection. Targets an empty / `IF NOT EXISTS` baseline, so **no introspection needed
   yet**.
5. **Per-project bin / `ddl_main!` macro** — the sprint-1 front-end. CLI verbs: `script`, `publish`,
   `export`, `import`.

**Out of scope this sprint (designed-for, not built)**

- Introspection (live DB → model).
- Diff engine / `compare` / incremental `ALTER` plans.
- Destructive-change policy enforcement (the `PublishOptions` seam is defined; teeth come with diff).
- Stub-compiling global CLI.

### API sketch (subject to change)

```rust
// squealy-model
pub struct DatabaseModel { /* schemas: Vec<SchemaModel>, ... */ }
impl DatabaseModel {
    pub fn from_database<D: Database>() -> Self;     // walk the derives
    pub fn read_package(path: &Path) -> io::Result<Self>;
    pub fn write_package(&self, path: &Path) -> io::Result<()>;
}

// core `squealy` — sibling to Backend; implemented by squealy-postgresql
pub trait SchemaBackend {
    fn render_create(&self, model: &DatabaseModel, out: &mut dyn Write) -> io::Result<()>;
    // future: fn introspect(...) -> DatabaseModel, fn render_alter(plan, ...)
}

pub struct PublishOptions { /* destructive-change policy; teeth in the diff sprint */ }

pub fn script<B: SchemaBackend>(model: &DatabaseModel, backend: &B) -> io::Result<String>;
pub async fn publish<B, C>(model: &DatabaseModel, backend: &B, conn: &C, opts: &PublishOptions)
    -> Result<(), Error>;
```

## Roadmap (post-sprint-1)

- **Introspection** (Postgres first): live DB → `DatabaseModel` via `information_schema`/`pg_catalog`.
- **Diff engine:** `compare(desired, actual)` → classified plan (safe / destructive / ambiguous),
  rename hints, type-change `USING` casts; `script`/`publish` consume the plan.
- **`PublishOptions` teeth:** configurable handling of drops / lossy changes (block, allow, generate-only).
- **Hybrid flow:** generate a reviewable upgrade script from two models (crate↔crate, package↔package,
  or model↔live), bridging declarative authoring with checked-in, auditable artifacts.
- **Stub-compiling global CLI** (`squealy-cli`) for a single-binary, sqlpackage-like UX.

## Implementation status (sprint 1)

Done and tested:
- **Owned model + walker** in core `squealy` (`DatabaseModel`/`SchemaModel`/`TableModel`/`ColumnModel`
  + named `Constraint`/`ForeignKeyModel`/`CheckModel`/`IndexModel`, owned `SqlType`/`DefaultValue`,
  deterministic constraint names, `DatabaseModel::from_database::<D>()`).
- **`SchemaBackend` trait** in core; **Postgres `render_create`** (phased create-from-scratch).
- **`.sqz` package** in `squealy-model`: deterministic KDL `model.kdl` + `manifest.kdl` in a zip,
  with full KDL and zip round-trip tests.
- **`render_create_sql` / `script`** engine entry points.
- **`publish`** — `DdlExecutor` trait in core; `PostgresConnection` implements it transactionally via
  `batch_execute` (behind the `schema` feature); `squealy-model::publish`/`publish_database` render
  then execute. Verified by a live `#[ignore]`d integration test (create-from-scratch → insert →
  select round-trip).
- **DDL is feature-gated**: `squealy-postgresql`'s `SchemaBackend`/`DdlExecutor` impls and the
  whole-DB renderer sit behind a default-off `schema` feature, so query-only users carry none of it.

Remaining for sprint 1:
- **`ddl_main!` macro + `cli`** — dispatch `script` / `export` / `import` / `publish`. `ddl_main!` can
  be a `macro_rules!` (no proc-macro): it expands to a `main` calling `cli::<D, B>(...)`. `publish`
  from the CLI additionally needs a connect step (URL → connection); the programmatic `publish` API
  is already done and decoupled from that.

## Settled decisions

- Source of truth = the Rust database crate; git over the Rust source is the schema history.
- Owned neutral `DatabaseModel` (+ `SqlType`/`DefaultValue`) and the `SchemaBackend` trait live in
  **core `squealy`** (see the placement note in Architecture); the `squealy-model` engine owns the
  operations and heavier deps.
- `DatabaseModel` uses **table-level named constraints**.
- Constraint names are deterministic (`pk_`/`fk_`/`uq_`/`ck_`/`idx_`) with optional override; they are
  the diff identity (name-based; rename hints later).
- `SqlType` stays minimal in sprint 1; grows structurally with introspection; package type encoding
  is structured from day one.
- Package = KDL in a zip, extension **`.sqz`** (`manifest.kdl` + `model.kdl`), backend-neutral,
  deterministic (store-only zip, fixed timestamp).
- Front-end = `squealy_model::cli::<D, B>()` + `ddl_main!(Db, Backend)` macro.
- **`SchemaBackend` trait in core `squealy`**; `squealy-postgresql` implements it; `write_table` is
  superseded (retirement deferred until nothing depends on it).

## Open questions

- `publish` execution seam: confirm `DdlExecutor` (raw `batch_execute`) as the capability, and whether
  publish wraps the script in a transaction (Postgres supports transactional DDL).
- Exact `PublishOptions` surface (deferred until the diff sprint gives it teeth).
- Whether `manifest.kdl` should also pin a minimum `squealy-version` for forward-compat.
- Hash algorithm for `model-hash` (blake3 vs sha256).
