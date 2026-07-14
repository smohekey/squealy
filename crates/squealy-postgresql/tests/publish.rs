//! Live end-to-end test: publish a schema carrying views, then re-plan against the freshly published
//! database and assert the plan is empty — the round-trip exit criterion (PR 2.1). This exercises the
//! reverse-parser view-body reconstruction: PostgreSQL's `pg_get_viewdef` deparse is read back into a
//! structural `ViewBody`, canonicalized (result pins folded), and compared against the crate model, so a
//! published view converges to a no-op instead of re-applying every run.
//!
//! `#[ignore]`d like the other backend integration tests; run with a database via:
//! `SQUEALY_POSTGRES_URL=... cargo test -p squealy-postgresql --test publish --features schema -- --ignored`.

use std::sync::OnceLock;

use squealy::*;
use squealy_postgresql::Postgres;
use tokio::sync::Mutex;

/// Serializes the live-database tests in this binary. They share one database and each re-plans by
/// introspecting the *whole* database, so two running concurrently — or one seeing the other's fixtures —
/// would diff each other's schemas. Each test holds this guard for its duration and resets both fixture
/// schemas first.
fn db_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Drops every fixture this binary creates — the off-path `publish_public` schema and the on-`public`
/// objects — so a test re-plans against only what it just published (whole-database introspection would
/// otherwise diff a sibling test's leftovers).
async fn reset_fixtures(connection: &mut squealy_postgresql::PostgresConnection) {
    connection
        .execute_ddl(
            "DROP SCHEMA IF EXISTS publish_public CASCADE;\n\
             DROP VIEW IF EXISTS public.sp_active_users;\n\
             DROP TABLE IF EXISTS public.sp_users",
        )
        .await
        .expect("reset fixtures");
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(PublishPublic)]
struct PublishUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    active: C::Type<'scope, bool>,
}

// A single-`SELECT` view over the table. Its body — projection, `WHERE`, and `FROM` — is exactly the
// shape `read_view_query` reconstructs from `pg_get_viewdef`.
#[allow(dead_code)]
#[derive(View)]
#[schema(PublishPublic)]
struct PublishActiveUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for PublishActiveUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<PublishUser>()
            .where_(|user| user.active.equals(true))
            .project(|(user,)| (user.id, user.name))
    }
}

// A view over the *first* view — its reconstructed body must recover the view-on-view dependency (the
// `FROM active_users` edge) so the diff orders live drops correctly. This is the edge `view_dependencies`
// (pg_rewrite) used to supply; a reconstructed body now supplies it by being walked.
#[allow(dead_code)]
#[derive(View)]
#[schema(PublishPublic)]
struct PublishNamedUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for PublishNamedUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<PublishActiveUser>()
            .project(|(active,)| (active.id, active.name))
    }
}

// A grouped/aggregated single-source view: `SELECT active, count(id) … GROUP BY active`. Its
// `pg_get_viewdef` dequalifies the `GROUP BY active` term to a bare source column (which the reverse
// parser must re-bind, not mistake for the projected output alias), and its `count(id)` carries a
// result pin — so this exercises both reconciliations end to end.
#[allow(dead_code)]
#[derive(View)]
#[schema(PublishPublic)]
struct PublishActiveCount<'scope, C: ColumnMode = ColumnExpr> {
    active: C::Type<'scope, bool>,
    count: C::Type<'scope, i64>,
}

impl<'scope, C: ColumnMode> ViewDefinition for PublishActiveCount<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<PublishUser>()
            .group_by(|(user,)| user.active)
            .project(|(user,)| (user.active, user.id.count()))
    }
}

#[allow(dead_code)]
#[derive(Schema)]
struct PublishPublic {
    users: PublishUser<'static, ColumnName>,
    #[view]
    active_users: PublishActiveUser<'static, ColumnName>,
    #[view]
    named_users: PublishNamedUser<'static, ColumnName>,
    #[view]
    active_counts: PublishActiveCount<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct PublishAppDatabase {
    public: PublishPublic,
}

// A schema mapped to `public` — which is on the default `search_path`, so `pg_get_viewdef` deparses its
// view's sources WITHOUT a schema qualifier. Introspection must still reconstruct `schema: Some("public")`
// (it empties the search_path so the deparse is fully qualified), else an on-path view churns every plan.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct SpUser<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(View)]
#[schema(Public)]
struct SpActiveUser<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for SpActiveUser<'scope, C> {
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
        db.from::<SpUser>().project(|(user,)| (user.id, user.name))
    }
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: SpUser<'static, ColumnName>,
    #[view]
    active_users: SpActiveUser<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct PublicDb {
    public: Public,
}

fn database_url() -> String {
    std::env::var("SQUEALY_POSTGRES_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:55432/squealy_test".to_owned())
}

#[tokio::test]
#[ignore]
async fn publishing_views_then_replanning_is_empty() {
    let _guard = db_lock().lock().await;
    let model = DatabaseModel::from_database::<PublishAppDatabase>();
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");

    reset_fixtures(&mut connection).await;

    squealy_model::publish(&model, &Postgres, &mut connection)
        .await
        .expect("publish create-from-scratch");

    // Re-planning the same crate model against the freshly published schema must converge to an empty
    // plan. Before PR 2.1, an introspected view carried an empty body, so the diff re-applied every view
    // as a `CREATE OR REPLACE VIEW` on every run; now the body is reconstructed from `pg_get_viewdef` and
    // compares equal (result pins canonicalized on both sides).
    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::default(),
    )
    .await
    .expect("re-plan against published schema");
    assert!(
        plan.steps.is_empty(),
        "expected empty plan after publishing views, got: {:?}",
        plan.steps
    );

    reset_fixtures(&mut connection).await;
}

#[tokio::test]
#[ignore]
async fn publishing_an_on_search_path_view_then_replanning_is_empty() {
    // The `public` schema is on the default `search_path`, so `pg_get_viewdef` deparses this view's source
    // as bare `FROM sp_users`. Introspection must still reconstruct `schema: Some("public")` (by emptying
    // the search_path so the deparse is fully qualified) for the re-plan to be empty.
    let _guard = db_lock().lock().await;
    let model = DatabaseModel::from_database::<PublicDb>();
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");

    reset_fixtures(&mut connection).await;

    squealy_model::publish(&model, &Postgres, &mut connection)
        .await
        .expect("publish create-from-scratch");

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::default(),
    )
    .await
    .expect("re-plan against published schema");
    assert!(
        plan.steps.is_empty(),
        "expected empty plan after publishing an on-search-path view, got: {:?}",
        plan.steps
    );

    reset_fixtures(&mut connection).await;
}

// A table exercising every neutral integer width PostgreSQL has no dedicated type for, so each renders to
// `smallint`/`integer`/`bigint`/`numeric` and must introspect back to the signed representative it
// canonicalizes to (`canonical_pg_sql_type`). Two unsigned columns carry defaults to exercise the matching
// `canonical_pg_default` fold (`DefaultValue::UInt` → the `Int` introspection reads back). The three
// natively-round-tripping widths (`i16`/`i32`/`i64`) are covered by the other fixtures.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(PublishInts)]
struct PublishIntWidths<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    tiny_signed: C::Type<'scope, i8>,      // smallint → I16
    tiny_unsigned: C::Type<'scope, u8>,    // smallint → I16
    small_unsigned: C::Type<'scope, u16>,  // integer  → I32
    medium_unsigned: C::Type<'scope, u32>, // bigint   → I64
    size_signed: C::Type<'scope, isize>,   // bigint   → I64
    size_unsigned: C::Type<'scope, usize>, // bigint   → I64
    big_unsigned: C::Type<'scope, u64>,    // numeric  → I128
    huge_signed: C::Type<'scope, i128>,    // numeric  → I128
    huge_unsigned: C::Type<'scope, u128>,  // numeric  → I128
    // An explicit arbitrary-precision numeric column reaches the model as `Raw("numeric")` and renders to
    // bare `numeric` too, so it must fold to the same `I128` representative introspection reads back.
    #[column(db_type = "numeric")]
    explicit_numeric: C::Type<'scope, i128>,
    #[column(default = value(7))]
    tiny_unsigned_default: C::Type<'scope, u8>,
    #[column(default = value(9))]
    big_unsigned_default: C::Type<'scope, u64>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct PublishInts {
    int_widths: PublishIntWidths<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct PublishIntsDatabase {
    ints: PublishInts,
}

#[tokio::test]
#[ignore]
async fn publishing_narrow_and_wide_integer_columns_then_replanning_is_empty() {
    // A `u8`/`u16`/`u32`/`u64`/`u128`/`i8`/`i128`/`isize`/`usize` column renders to one of PostgreSQL's
    // four integer types (lossily — twelve neutral widths, four PG types). Before this fix, introspection
    // read each back as a different width (`smallint` → I16, bare `numeric` → `Raw("numeric")`), so the
    // diff re-issued a spurious `AlterColumn` on every plan. Now the desired widths canonicalize to the
    // signed representative introspection yields, and the round-trip converges to an empty plan.
    let _guard = db_lock().lock().await;
    let model = DatabaseModel::from_database::<PublishIntsDatabase>();
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_ints CASCADE")
        .await
        .expect("reset int-widths fixture");

    squealy_model::publish(&model, &Postgres, &mut connection)
        .await
        .expect("publish create-from-scratch");

    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::default(),
    )
    .await
    .expect("re-plan against published schema");
    assert!(
        plan.steps.is_empty(),
        "expected empty plan after publishing narrow/wide integer columns, got: {:?}",
        plan.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_ints CASCADE")
        .await
        .expect("clean up int-widths fixture");
}
