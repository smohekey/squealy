# DDL Management — Design

Status: **draft / implementation notes** · Branch: `codex/schema-introspection`

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
| Extract (DB → dacpac) | introspect live DB → model/package |
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
  │ live DB               │── read ──┘                      └──────────────────────────┘
  └──────────────────────┘                                   * = future sprint
```

### The owned model (in core `squealy`)

Runtime/serializable mirror of the compile-time trait model. "Schema" here means **namespace**,
consistent with `#[derive(Schema)]`.

> **Placement (revised during implementation):** the model types *and* the `SchemaBackend` trait live
> in **core `squealy`**, not `squealy-model`. `SchemaBackend::render_create` must reference
> `DatabaseModel`, future plan rendering must reference `DatabasePlan`, and backends must implement
> both without depending on the management engine —
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
the table-level lists), deserialize from a package, or introspect a live DB. Every operation
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
                                  + neutral plan data (DatabasePlan, DatabasePlanStep, …)
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

### Front-end — a stub-compiling global CLI

**Decision: the front-end is a standalone `squealy` CLI that drives the user's crate; the user's crate
stays pristine.** No `main`, no attribute, no derive change, and the user can keep
`#![forbid(unsafe_code)]`.

Rejected alternatives and why:
- *In-crate `run_cli`/`ddl_main!`*: still requires the user to write a `main`/macro call — boilerplate.
- *Reflection registry (`linkme`/`inventory`)*: would auto-register each `#[derive(Database)]`, but the
  registration emits `#[link_section]` — an **unsafe attribute** — *into the user's crate*, breaking
  any user with `#![forbid(unsafe_code)]`. That's exactly the "magic in the crate" we're avoiding.

How the CLI works (the model only materializes by *running* code — trait-resolved column types + owned
strings — so the crate must be compiled and run):

1. The operator names the database type: `squealy <cmd> --database my_crate::AppDatabase` (optionally
   defaulted via `[package.metadata.squealy] database = "…"`). Rust has no reflection, so *something*
   must name the type; the operator already knows it. Multiple databases → name which one. (Auto-`list`
   would need nightly rustdoc JSON — deferred.)
2. The CLI generates a tiny **stub crate** depending on the user crate + `squealy-model`, whose `main`
   is `from_database::<my_crate::AppDatabase>()` → serialize → write to a CLI-controlled file.
3. `cargo build` + run the stub in a **subprocess**; harvest the model from the file.

**Security** (the stub runs the user's own code — same trust as `cargo run` — but with sharp edges):
- **Validate `--database` as a strict Rust path** (`ident(::ident)*`, no generics/whitespace/punct).
  It's interpolated into generated source, so an untrusted value (CI/multi-tenant) would be code
  injection. Mandatory.
- Use a **private, unpredictable temp dir** for the stub (avoid TOCTOU).
- Harvest via a **CLI-controlled file**, not stdout (avoid `println!`/panic spoofing).
- A **subprocess** isolates user code from the CLI process (a point in the stub's favor over the
  rejected `dlopen` path, which would have run user code *in* the CLI).

**Export-then-publish split (privilege separation):**
- `squealy export --database … model.sqz` — compile + run stub → package. **No DB, no secrets.**
  Add `--refactors refactor.kdl` to embed explicit rename/refactor intent in the output package.
- `squealy publish --package model.sqz --url …` — operates on the static artifact; **executes no
  project code**, just renders DDL and runs it. By default this is create-from-scratch DDL; with
  `--incremental` it introspects the live database, reads any embedded `refactor.kdl`, builds a
  policy-checked plan, and applies that plan; add `--report` to print that plan without executing
  it. Right shape for CI/CD with credentials.
- `squealy status --package model.sqz --url …` — read-only comparison between a desired package and
  live database state. It prints whether schema diff is clean/changed and includes refactor metadata
  status for the package log, plus package metadata match/mismatch/missing lines.
- `squealy refactors list --url …` — read-only operator view of the applied refactor ids recorded
  in the backend metadata table. Add `--package model.sqz` to compare recorded ids with the package
  `refactor.kdl` and print `applied`, `pending`, or `recorded-only` status lines.
- `squealy refactors repair --package model.sqz --url …` — metadata-only recovery path for cases
  where a package refactor's final schema state is already present but the backend metadata row is
  missing. It validates the live final state before recording the id.
- `squealy publish --database … --url …` (compile+run then deploy in one step) stays available for dev
  convenience, using `SchemaConnect` to open the connection from the URL.

## Package format — KDL in a zip (`.sqz`)

A deploy artifact, *not* the committed source of truth. Direct git-diff prettiness is a nice-to-have,
not a requirement (git tracks the Rust source).

**Container:** zip (the dacpac approach). Single distributable file with room to grow
(pre/post scripts, multiple namespaces) later.

```
package.sqz (zip)
├── manifest.kdl     // metadata, read without parsing the whole model
├── model.kdl        // the DatabaseModel
└── refactor.kdl     // optional explicit rename/refactor operations
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

**`refactor.kdl`:**

Refactors are explicit deployment intent that cannot be inferred safely from schema snapshots. For
example, a removed table plus an added table may be a rename or a real replacement. The package can
carry an optional refactor log; packages without it read as an empty log.

```kdl
refactors {
  rename-table id="2026-rename-users" schema="public" from="app_users" to="users"
  rename-column id="2026-rename-user-name" schema="public" table="users" from="display_name" to="name"
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
5. **Stub-compiling global CLI** — the sprint-1 front-end. CLI verbs: `check`, `script`, `export`,
   `introspect`, `publish`, and `capabilities`.

**Out of scope this sprint (designed-for, not built)**

- Rename hints and richer ambiguous-change handling, such as type-change `USING` casts and
  backend-specific identity/generated-column transitions.

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
    fn render_plan(&self, plan: &DatabasePlan, out: &mut dyn Write) -> io::Result<()>;
}

pub struct DiffPolicy {
    pub allow_destructive: bool,
    pub allow_ambiguous: bool,
}

pub fn script<B: SchemaBackend>(model: &DatabaseModel, backend: &B) -> io::Result<String>;
pub fn render_plan_sql<B: SchemaBackend>(plan: &DatabasePlan, backend: &B) -> io::Result<String>;
pub fn diff_models(desired: &DatabaseModel, actual: &DatabaseModel) -> DatabaseDiff;
pub fn plan_models(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
    policy: DiffPolicy,
) -> Result<DatabasePlan, DiffPolicyError>;
pub async fn plan_from_database<C: SchemaIntrospect>(
    desired: &DatabaseModel,
    conn: &mut C,
    policy: DiffPolicy,
) -> Result<DatabasePlan, PlanFromDatabaseError<C::Error>>;
pub async fn apply_plan<B, C>(plan: &DatabasePlan, backend: &B, conn: &mut C) -> Result<(), Error>
where
    B: SchemaBackend,
    C: DdlExecutor;
pub async fn publish<B, C>(model: &DatabaseModel, backend: &B, conn: &C) -> Result<(), Error>;
```

## Roadmap (post-sprint-1)

- **Richer ALTER rendering:** rename hints, backend-specific assists for ambiguous changes such as
  type-change `USING` casts, and identity/generated-column transitions where a backend can apply
  them safely.
- **Hybrid flow:** generate a reviewable upgrade script from two models (crate↔crate, package↔package,
  or model↔live), bridging declarative authoring with checked-in, auditable artifacts.
- **Schema compare CLI** for desired-vs-live and desired-vs-package workflows.

## Implementation status (sprint 1)

Done and tested:
- **Owned model + walker** in core `squealy` (`DatabaseModel`/`SchemaModel`/`TableModel`/`ColumnModel`
  + named `Constraint`/`ForeignKeyModel`/`CheckModel`/`IndexModel`, owned `SqlType`/`DefaultValue`,
  deterministic constraint names, `DatabaseModel::from_database::<D>()`).
- **Neutral plan data** in core `squealy` (`DatabasePlan`/`DatabasePlanStep`/`TablePlanStep`) so
  backend crates can render incremental plans without depending on `squealy-model`.
- **`SchemaBackend` trait** in core; **Postgres and MySQL `render_create`** (phased
  create-from-scratch).
- **Schema capabilities** in core: backends report the metadata they can render and introspect as a
  full round-trip. `squealy-model::check_create` preflights models against those capabilities before
  rendering, and `squealy capabilities --backend <postgres|mysql>` prints the current support matrix.
- **`.sqz` package** in `squealy-model`: deterministic KDL `model.kdl` + `manifest.kdl` in a zip,
  optional `refactor.kdl` explicit rename/refactor log, with full KDL and zip round-trip tests.
- **`render_create_sql` / `script`** engine entry points.
- **`publish`** — `DdlExecutor` trait in core; Postgres and MySQL connections implement it;
  `squealy-model::publish`/`publish_database` render then execute. Verified by live `#[ignore]`d
  integration tests that publish and introspect the resulting schema.
- **DDL is feature-gated** for `squealy-postgresql`: schema-management impls and the whole-DB
  renderer sit behind a default-off `schema` feature, so query-only users carry none of it.

- **`SchemaConnect`** (core trait) + Postgres/MySQL impls — opens a connection from a URL; used by
  `publish --database … --url …` and `publish --package … --url …`.
- **Stub-compiling `squealy` CLI** (`squealy-cli`, bin `squealy`): resolves the package via
  `cargo metadata`, validates `--database` as a strict Rust path, generates a stub in a private temp
  dir, compiles + runs it as a subprocess, and harvests the `.sqz`. Commands `check` / `script` /
  `export` / `diff` / `plan` / `introspect` / `publish` / `capabilities`; model-taking commands are sourced from
  `--database <path>` (compile + run) or `--package <file.sqz>` (no project code runs).
  `introspect --backend <backend> --url <url> <output.sqz>` exports a live database to a package.
  `--backend postgres|mysql` selects backend-specific render/check/introspect/publish behavior.
- **Introspection**: Postgres and MySQL schema-management connections implement
  `SchemaIntrospect`; the neutral model preserves richer schema facts such as structured types,
  identity/generated columns, foreign-key actions/deferrability/validation, index methods, index
  expressions, include columns, collations, operator classes, and predicates where the backend can
  round-trip them.
- **Diffing and policy**: `diff_models(desired, actual)` compares packages/models by stable names,
  classifies changes as safe/destructive/ambiguous, and `DiffPolicy` can block risky changes.
  `squealy diff` prints risk-prefixed changes and can enforce policy with `--check-policy`,
  `--allow-ambiguous`, and `--allow-destructive`.
- **Planning**: `squealy-model::plan_models(desired, actual, policy)` turns a diff into ordered,
  policy-checked, backend-neutral `DatabasePlan` steps. Backend-specific ALTER rendering/execution is
  the next layer.
- **Refactor-aware planning**: `plan_models_with_refactors` consumes explicit `refactor.kdl`
  operations and turns matching drop/add pairs into safe table/column rename plan steps, preserving
  follow-up alterations when the renamed object also changed shape.
- **Refactor metadata**: rename plan steps produced from `refactor.kdl` carry the stable operation id.
  Postgres and MySQL write those ids to an internal `__squealy.refactors` metadata table during
  incremental plan rendering, and expose `SchemaRefactorStore` to read the recorded ids back.
- **Package metadata**: successful publish records current package facts in `__squealy.metadata`,
  currently package format version, canonical package content hash, and `squealy-model` crate
  version. The hash is a deterministic fingerprint of canonical `manifest.kdl`, `model.kdl`, and
  optional `refactor.kdl`; it is for drift visibility, not a security primitive.
- **Applied-refactor filtering**: `plan_from_database_with_refactors` reads recorded ids, validates
  that each recorded refactor's obvious final state is present in the live model, filters those
  operations from the package refactor log, and then plans with the remaining refactors.
- **Refactor metadata CLI**: `squealy refactors list --backend <backend> --url <url>` prints
  recorded applied refactor ids. With `--package <file.sqz>` it compares backend metadata with the
  package refactor log so operators can see which refactors are applied, pending, or recorded
  outside the package. `squealy refactors repair --package <file.sqz> --url <url>` validates that
  package refactors already match live final schema state and records missing metadata ids without
  mutating application schema.
- **Plan rendering**: core `SchemaBackend::render_plan` lets backends render neutral plans without
  depending on `squealy-model`; Postgres and MySQL implement create/drop/add-column/index/constraint
  plan rendering, table/column rename rendering, plus changed column rendering for type, collation,
  nullability, default, and comment differences. Identity/generated-column transitions and cast hints
  remain explicit unsupported cases. `squealy-model::render_plan_sql` is the allocating convenience
  wrapper.
- **Plan/apply engine APIs**: `plan_from_database` introspects live state and returns a policy-checked
  plan; `apply_plan` renders that plan through the selected backend and executes the resulting DDL.
- **Plan CLI**: `squealy plan --backend <backend> --desired desired.sqz --actual actual.sqz`
  renders incremental DDL between packages and enforces `DiffPolicy` by default. The desired package
  can carry `refactor.kdl`; matching drop/add pairs are rendered as safe rename steps.
- **Status CLI**: `squealy status --backend <backend> --package <file.sqz> --url <url>` introspects
  live schema state, compares it with the desired package, validates recorded refactor metadata
  against the live final state, compares package metadata with `__squealy.metadata`, and prints
  schema/refactor/metadata status without applying changes.
- **Incremental publish CLI**: `squealy publish --incremental ...` introspects the live database,
  reads package refactors when publishing from `--package`, builds a policy-checked plan, and applies
  it. Ambiguous/destructive changes remain blocked unless explicitly allowed. `--report` renders the
  live plan to stdout without applying it.

**Sprint 1 is functionally complete, and the diff/policy/neutral-plan layer is underway.** Next:
identity/generated-column transition handling, rename/cast hints, and the hybrid reviewable-script
flow.

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
- Front-end = a **standalone stub-compiling `squealy` CLI**; the user's crate stays pristine (no
  boilerplate, keeps `forbid(unsafe_code)`). Database type named via `--database <path>`. Reflection
  registry (`linkme`) rejected — it injects unsafe `#[link_section]` into the user crate.
- **Privilege split**: `export` (compile+run → `.sqz`, no secrets) then `publish --package` (no code
  execution); `publish --database` stays for dev convenience.
- **Backend selection is explicit** for management commands via `--backend`, defaulting to Postgres.
- **Capability support means full render + introspection round-trip support**, not only SQL syntax.
  Backends that cannot round-trip a feature should report it unsupported and let `check_create` fail
  before rendering.
- **`SchemaBackend` trait in core `squealy`**; backend crates implement it; `write_table` is
  superseded (retirement deferred until nothing depends on it).

## Open questions

- Exact `PublishOptions` surface (deferred until the diff sprint gives it teeth).
- Whether `manifest.kdl` should also pin a minimum `squealy-version` for forward-compat.
- Hash algorithm for `model-hash` (blake3 vs sha256).
