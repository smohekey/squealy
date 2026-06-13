# Squealy — Repository Review & Hardening/Completion Roadmap

> Status: review as of branch `codex/schema-introspection`. This is an assessment and a
> prioritized roadmap — it changes no behavior. The companion design doc is
> [ddl-management.md](ddl-management.md).

## 1. Executive summary

Squealy is a DACFx-style schema-management toolchain: the Rust database crate is the source
of truth, and KDL/zip `.sqz` packages are derived, deterministic artifacts. Sprint-1 is
**functionally complete and well-tested** — owned neutral model, `.sqz` packaging,
create-from-scratch DDL, diff/policy, refactor-aware neutral planning, incremental publish,
status reporting, and full Postgres + MySQL introspection, all behind a green CI path.

The architecture is clean: the neutral `DatabaseModel` and the `SchemaBackend` trait live in
core `squealy`, the heavier diff/plan/package engine lives in `squealy-model`, and each
backend owns its own SQL rendering. There are **no `TODO`/`FIXME`/`unimplemented!()` markers
in non-test code**, and every CLI command is wired.

The principal risks are **operational, not correctness**: opaque error types, a
non-transactional MySQL apply path that can leave a half-migrated schema, and connection
URLs (with embedded passwords) flowing unredacted into error messages. None block correct
use today, but each is a sharp edge for a tool that mutates production databases.

**What's left to finish** falls into three buckets: hardening the sprint-1 surface
(Section 3), closing the documented feature gaps — identity/generated-column transitions,
cast hints, hybrid reviewable-script flow (Section 4) — and adding publish-time operational
safety (Section 5). Section 6 merges these into a single P0/P1/P2 action list. New backends
(SQLite/SQL Server) are explicitly out of scope for this roadmap.

## 2. Architecture & completeness map

### Workspace (7 crates)

| Crate | Responsibility |
|-------|----------------|
| `squealy` | Core: AST, query builders, the owned neutral `DatabaseModel`, `SqlType`, `SchemaBackend`/`DdlExecutor`/`SchemaConnect` traits, neutral `DatabasePlan`. |
| `squealy-macros` | `#[derive(Table/Schema/Database/ColumnType)]`. |
| `squealy-model` | DDL engine: `.sqz` package (KDL/zip), diff, plan, refactor log, publish/metadata/history. |
| `squealy-postgresql` | Postgres query + schema backend (DDL/introspection behind the `schema` feature). |
| `squealy-mysql` | MySQL schema-only backend (no query layer, by design). |
| `squealy-test` | In-memory backend for deterministic SQL-rendering tests. |
| `squealy-cli` | The `squealy` binary: stub-compiles the user crate or reads a `.sqz`. |

### Data flow

```
#[derive] types ──walk──▶ DatabaseModel ──serialize──▶ .sqz (manifest/model/refactor KDL in zip)
                              │                              │
   live DB ──introspect──────┘                              ├─ diff_models ─▶ plan ─▶ render/apply
                                                            └─ check_create (capability preflight)
```

### Completeness

Every CLI verb is implemented and dispatched in
[crates/squealy-cli/src/main.rs:33-164](../crates/squealy-cli/src/main.rs#L33):
`capabilities`, `check`, `script`, `export`, `diff`, `plan`, `introspect`, `status`,
`publish`, and `refactors {list,repair}`. Backends implement create/drop/add-column/
index/constraint/rename rendering plus changed-column rendering (type, collation,
nullability, default, comment).

**Explicitly deferred / unsupported** (by design, per
[ddl-management.md](ddl-management.md)):

- Constraint `enforcement` is hardcoded `None` during introspection
  ([crates/squealy-postgresql/src/introspect.rs:259](../crates/squealy-postgresql/src/introspect.rs#L259),
  [:312](../crates/squealy-postgresql/src/introspect.rs#L312)).
- Identity / generated-column transitions and type-change cast hints remain **explicit
  unsupported cases** in `render_plan` for both backends.

## 3. Hardening findings

### 3a. Error handling

- **Opaque CLI error type.** The CLI threads `Result<T, String>` end-to-end
  ([crates/squealy-cli/src/main.rs:219](../crates/squealy-cli/src/main.rs#L219) and throughout
  `run`), so errors aren't programmatically inspectable and source chains are flattened into
  prose. Recommend structured `thiserror` enums per crate, with string formatting reserved
  for the final sink ([main.rs:258](../crates/squealy-cli/src/main.rs#L258)).
- **Swallowed Postgres connection errors.** `connect` spawns a fire-and-forget IO task that
  discards the connection result
  ([crates/squealy-postgresql/src/lib.rs:178-180](../crates/squealy-postgresql/src/lib.rs#L178)):
  `tokio::spawn(async move { let _ = connection.await; })`. A dropped or failed connection
  surfaces only on the *next* query, as a confusing downstream error. Recommend capturing the
  join handle and/or routing the error into a shared slot or `tracing` event.
- **`unwrap()` in SQL rendering.** `String::from_utf8(out).unwrap()` appears in the rendering
  convenience wrappers
  ([crates/squealy-postgresql/src/sql.rs:2203](../crates/squealy-postgresql/src/sql.rs#L2203),
  [crates/squealy-mysql/src/sql.rs:727](../crates/squealy-mysql/src/sql.rs#L727)), alongside the
  sink `.unwrap()`s in the select-lowering path
  ([sql.rs:384-386](../crates/squealy-postgresql/src/sql.rs#L384)). These are safe in practice
  (we only ever write UTF-8 into the buffer), but the invariant should be documented or the
  error propagated rather than panicking.
- **Panic-based test helpers.** `panic!("table/column not found")` lives in model lookup
  helpers ([crates/squealy/src/model.rs:714](../crates/squealy/src/model.rs#L714),
  [:779](../crates/squealy/src/model.rs#L779)). Confirm these remain test-only and are never
  reachable from a published API path.

### 3b. Operational safety

- **MySQL apply is not transactional.** `execute_ddl` splits the batch and runs each
  statement independently
  ([crates/squealy-mysql/src/lib.rs:114-122](../crates/squealy-mysql/src/lib.rs#L114)), whereas
  Postgres wraps the whole batch in a transaction
  ([crates/squealy-postgresql/src/lib.rs:162-167](../crates/squealy-postgresql/src/lib.rs#L162)).
  MySQL DDL auto-commits, so a mid-batch failure leaves a **partially-applied schema** with no
  automatic rollback. This asymmetry must be documented loudly. Mitigations: `check_create`
  already preflights capability/validity before any statement runs; add per-statement progress
  reporting and a documented manual-recovery path; consider failing closed with a clear
  "schema may be partially applied" message that names the last successful statement.
- **Credential leakage in errors.** Connection URLs embed passwords and flow verbatim into
  error strings via `format!("connect: {error}")` across the CLI
  ([crates/squealy-cli/src/main.rs](../crates/squealy-cli/src/main.rs); see the `connect`
  call sites in `Introspect`/`Status`/`Publish`/`Refactors`), and the underlying
  `tokio_postgres`/`mysql_async` errors can echo the DSN. Recommend a redaction helper applied
  before any error or log emission.
- **No connection-string validation.** The `url` argument is passed straight to the driver
  ([crates/squealy-cli/src/main.rs:105](../crates/squealy-cli/src/main.rs#L105) and the other
  `--url` args). Recommend parsing + validating scheme/host up front with an actionable error
  (and redacting the value in any failure message).
- **TLS posture.** Postgres connects with `NoTls`
  ([crates/squealy-postgresql/src/lib.rs:176](../crates/squealy-postgresql/src/lib.rs#L176));
  MySQL uses rustls. Document the Postgres no-TLS default and provide an opt-in TLS path for
  remote targets.
- **Idempotency (strength).** DDL consistently uses `IF [NOT] EXISTS`, so re-runs of
  create-from-scratch are safe — worth stating explicitly as a deliberate property.

### 3c. CI & supply chain

- Current CI ([.github/workflows/ci.yml](../.github/workflows/ci.yml)) runs `cargo fmt
  --check`, `cargo rdme --check`, a full build, unit tests, and the `#[ignore]`d integration
  tests against PostgreSQL 17 and MySQL 8. This is a solid baseline.
- **Missing gates:** no `cargo clippy --all-targets -- -D warnings`, no `cargo audit`
  (advisory DB), no `cargo deny` (license/dup/source policy), and no MSRV job even though the
  workspace declares `rust-version = "1.85"`. Recommend adding all four; clippy and audit are
  the highest value and lowest cost.
- **`#![forbid(unsafe_code)]` (strength).** Present across crates — keep it, and consider a CI
  assertion that it stays present.

### 3d. Input safety & observability

- **No input-size bounds on package parsing.** `read_package_from` opens the zip and reads
  fixed entries, then `KdlDocument::parse_v2` parses them
  ([crates/squealy-model/src/package.rs:159-163](../crates/squealy-model/src/package.rs#L159),
  [:89](../crates/squealy-model/src/package.rs#L89),
  [:224](../crates/squealy-model/src/package.rs#L224)). A hostile `.sqz` (zip bomb / huge KDL)
  could exhaust memory. Recommend max-size guards on each entry before `read_to_string` /
  `parse_v2`.
- **No zip-slip risk (strength).** Entries are accessed by fixed name (`MODEL_ENTRY`,
  `REFACTOR_ENTRY`, manifest) via `archive.by_name(...)`
  ([package.rs:162](../crates/squealy-model/src/package.rs#L162),
  [:174](../crates/squealy-model/src/package.rs#L174)); nothing is extracted to a
  caller-controlled path. Note this as safe by construction.
- **No structured logging.** The tool uses ad-hoc `println!`/`eprintln!` throughout and has no
  `tracing`/`log` dependency. For a tool that mutates production schemas, recommend adopting
  `tracing` with a redaction layer (ties directly to the credential-leakage item in 3b) so
  publish operations are auditable and machine-parseable.

## 4. Completion roadmap — documented feature gaps

1. **Identity / generated-column transitions** *(highest complexity)*. Today these are
   explicit unsupported cases in both backends' `render_plan`. Closing the gap requires
   backend-specific ALTER sequences (e.g. Postgres `ADD/DROP IDENTITY`,
   `ALTER COLUMN … ADD GENERATED …`; regenerating stored generated columns) and careful risk
   classification, since several transitions are inherently rewriting/destructive. Build on the
   existing changed-column rendering path.
2. **Rename & cast hints** *(medium)*. The `refactor.kdl` log already drives table/column
   renames ([crates/squealy-model/src/refactor.rs](../crates/squealy-model/src/refactor.rs)).
   Extend the hint vocabulary to carry type-change `USING` casts so an ambiguous type change
   becomes a plannable, explicitly-authored step instead of a drop/add.
3. **Hybrid reviewable-script flow** *(medium)*. Generate an auditable upgrade script from any
   two model sources (crate↔crate, package↔package, model↔live), bridging declarative authoring
   with checked-in, reviewable artifacts. Builds directly on `render_plan_sql`; mostly a CLI +
   source-resolution surface on top of machinery that already exists.

## 5. Publish-safety roadmap

Operational guardrails for applying changes to live databases:

- **First-class dry-run.** Make "render the plan without applying" always available. Today
  `--report` requires `--incremental`
  ([crates/squealy-cli/src/main.rs:446](../crates/squealy-cli/src/main.rs#L446)); a
  create-from-scratch dry-run against a live target has no direct switch.
- **Lock & statement timeouts.** Set `lock_timeout` / `statement_timeout` (Postgres) and the
  MySQL equivalents on the publish session so a migration can't block indefinitely behind a
  held lock.
- **Explicit destructive confirmation.** Beyond the boolean `--allow-destructive` flag, require
  an interactive confirmation or an explicit `--yes` for destructive plans, echoing the exact
  destructive steps first.
- **MySQL partial-failure remediation** (ties to 3b): per-statement progress output plus a
  documented recovery procedure when a non-transactional apply fails mid-batch.
- **Online / low-lock DDL strategies.** Document and, where feasible, prefer low-lock paths per
  backend (e.g. Postgres `CREATE INDEX CONCURRENTLY`), noting where they conflict with
  transactional apply.

## 6. Prioritized action list

Status legend: ✅ done · ⬜ outstanding.

| Priority | Item | Section | Status |
|----------|------|---------|--------|
| **P0** | Redact credentials from all error/log output (URL redaction helper) | 3b | ✅ |
| **P0** | Document MySQL non-transactional apply; add per-statement progress + recovery guidance | 3b / 5 | ✅ |
| **P0** | Add `cargo clippy -D warnings` and `cargo audit` to CI | 3c | ✅ |
| **P0** | Surface (stop swallowing) Postgres connection-task errors | 3a | ✅ |
| **P1** | Introduce structured `thiserror` error types across crates | 3a | ⬜ |
| **P1** | Bound `.sqz` entry sizes before KDL parse | 3d | ⬜ |
| **P1** | Adopt `tracing` with a redaction layer for publish operations | 3d | ⬜ |
| **P1** | Validate connection strings up front (redacted on failure) | 3b | ⬜ |
| **P1** | First-class publish dry-run + destructive-change confirmation | 5 | ⬜ |
| **P1** | Add `cargo deny` + MSRV (1.85) CI jobs | 3c | ⬜ |
| **P1** | Lock/statement timeouts on the publish session | 5 | ⬜ |
| **P2** | Identity / generated-column transition planning | 4 | ⬜ |
| **P2** | Type-change cast hints in `refactor.kdl` | 4 | ⬜ |
| **P2** | Hybrid reviewable-script flow | 4 | ⬜ |
| **P2** | Online/low-lock DDL strategies (e.g. `CONCURRENTLY`) | 5 | ⬜ |
| **P2** | Document/propagate the `from_utf8` invariant in SQL rendering | 3a | ⬜ |

The P0/P1/P2 split is the recommended execution order for a follow-up implementation pass and
should be treated as the acceptance checklist for that work.
