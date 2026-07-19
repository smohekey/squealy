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
             DROP TABLE IF EXISTS public.sp_users;\n\
             DROP VIEW IF EXISTS public.ca_v;\n\
             DROP TABLE IF EXISTS public.ca_events",
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

/// A hand-built model for a view whose `ORDER BY` names a **projection output alias** — a shape the typed
/// `#[view]` builder cannot express (its clauses reference source columns), so it is built directly. On
/// PostgreSQL `pg_get_viewdef` deparses the standalone `ORDER BY total` to the underlying expression
/// (`ORDER BY (amount * 2)`); the source table also carries a colliding `total` column (a standalone alias
/// still wins). The clause-alias canonicalizer (git-bug 823ae69) must converge the two so the re-plan is
/// empty. Placed in `public` so it also exercises the search-path qualifier recovery.
fn clause_alias_model() -> DatabaseModel {
    fn column(name: &str) -> ColumnModel {
        ColumnModel {
            name: name.to_owned(),
            comment: None,
            ty: SqlType::I64,
            collation: None,
            nullable: true,
            default: None,
            identity: None,
            generated: None,
            on_update: None,
        }
    }
    let amount_times_two = ExprNode::Binary {
        op: ArithmeticOp::Multiply,
        left: Box::new(ExprNode::Column {
            alias: "q".to_owned(),
            column: "amount".to_owned(),
        }),
        right: Box::new(ExprNode::Literal("2".to_owned())),
    };
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: vec![TableModel {
                name: "ca_events".to_owned(),
                comment: None,
                columns: vec![column("amount"), column("total")],
                primary_key: None,
                foreign_keys: vec![],
                uniques: vec![],
                checks: vec![],
                indexes: vec![],
            }],
            views: vec![ViewModel {
                name: "ca_v".to_owned(),
                comment: None,
                columns: vec![ViewColumnModel {
                    name: "total".to_owned(),
                    ty: SqlType::I64,
                    nullable: true,
                }],
                query: ViewBody::Select(Box::new(ViewQueryModel {
                    projection: vec![ProjectionItem {
                        output_name: "total".to_owned(),
                        internal_alias: Some("total".to_owned()),
                        expr: amount_times_two,
                    }],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: Some("public".to_owned()),
                        name: "ca_events".to_owned(),
                        alias: "q".to_owned(),
                    })),
                    order_by: vec![OrderItem {
                        expr: ExprNode::BareColumn {
                            column: "total".to_owned(),
                        },
                        direction: None,
                        nulls: None,
                    }],
                    ..Default::default()
                })),
            }],
            enums: Vec::new(),
            sequences: Vec::new(),
        }],
    }
}

#[tokio::test]
#[ignore]
async fn publishing_a_clause_alias_view_then_replanning_is_empty() {
    let _guard = db_lock().lock().await;
    let model = clause_alias_model();
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
        "expected empty plan after publishing a clause-alias view, got: {:?}",
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

/// A `publish_enums` schema with a `mood` enum of `labels` and a table with a `mood`-typed column.
/// Enums are not expressible through the derive macro, so this is hand-built.
fn enum_fixture(labels: &[&str]) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("publish_enums".to_owned()),
            tables: vec![TableModel {
                name: "readings".to_owned(),
                comment: None,
                columns: vec![
                    ColumnModel {
                        name: "id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: Some(IdentityModel {
                            mode: IdentityMode::ByDefault,
                        }),
                        generated: None,
                        on_update: None,
                    },
                    ColumnModel {
                        name: "m".to_owned(),
                        comment: None,
                        ty: SqlType::Enum("mood".to_owned()),
                        collation: None,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                        on_update: None,
                    },
                ],
                primary_key: Some(Constraint {
                    prefix_lengths: Vec::new(),
                    name: "pk_readings".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
            views: Vec::new(),
            enums: vec![EnumModel {
                name: "mood".to_owned(),
                labels: labels.iter().map(|s| s.to_string()).collect(),
            }],
            sequences: Vec::new(),
        }],
    }
}

#[test]
fn creating_an_enum_column_without_a_declared_type_is_rejected() {
    // A bare `SqlType::Enum("mood")` column names an enum in its own schema; if no such enum is
    // declared, the create preflight must reject it rather than emit a table referencing a type that
    // was never created. (No database needed — this is a render-time preflight.)
    let mut model = enum_fixture(&["sad", "ok", "happy"]);
    model.schemas[0].enums.clear();
    let error = squealy_model::render_create_sql(&model, &Postgres)
        .expect_err("an enum column with no declared type must be rejected");
    assert!(error.to_string().contains("mood"), "{error}");
}

#[test]
fn creating_a_same_named_relation_and_enum_is_rejected() {
    // PostgreSQL owns a composite type per relation, so a table and an enum cannot share a name even in
    // a single create-from-scratch model. The create preflight must reject it.
    let mut model = enum_fixture(&["sad", "ok", "happy"]);
    model.schemas[0].tables[0].name = "mood".to_owned();
    let error = squealy_model::render_create_sql(&model, &Postgres)
        .expect_err("a relation sharing an enum name must be rejected");
    assert!(error.to_string().contains("mood"), "{error}");
}

#[test]
fn creating_a_view_column_of_an_undeclared_enum_is_rejected() {
    // The enum-declaration check must cover view output columns, not just table columns. The view has a
    // fully renderable body (so the only reason to reject it is the undeclared enum output column), and
    // the table's own enum column is removed so the collision is isolated to the view.
    let mut model = enum_fixture(&["sad", "ok", "happy"]);
    model.schemas[0].enums.clear();
    model.schemas[0].tables[0]
        .columns
        .retain(|column| column.name == "id");
    model.schemas[0].views.push(ViewModel {
        name: "reading_ids".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "id".to_owned(),
            ty: SqlType::Enum("mood".to_owned()),
            nullable: false,
        }],
        query: ViewBody::Select(Box::new(ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                internal_alias: None,
                expr: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "id".to_owned(),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: Some("publish_enums".to_owned()),
                name: "readings".to_owned(),
                alias: "q0_0".to_owned(),
            })),
            joins: Vec::new(),
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })),
    });
    let error = squealy_model::render_create_sql(&model, &Postgres)
        .expect_err("a view column of an undeclared enum must be rejected");
    assert!(error.to_string().contains("mood"), "{error}");
}

#[test]
fn incremental_rendering_rejects_an_undeclared_enum_column() {
    // The incremental render path does not run `check_create`, so it must independently reject a column
    // typed as an enum the desired schema never declares — otherwise offline `squealy plan` emits a
    // qualified type reference that fails at execution.
    let mut model = enum_fixture(&["sad", "ok", "happy"]);
    model.schemas[0].enums.clear();
    let plan = squealy_model::DatabasePlan { steps: Vec::new() };
    let error = squealy_model::render_plan_sql(&plan, &model, &Postgres)
        .expect_err("an incremental plan referencing an undeclared enum must be rejected");
    assert!(error.to_string().contains("mood"), "{error}");
}

#[tokio::test]
#[ignore]
async fn publishing_an_enum_then_replanning_is_empty() {
    // A published enum type + a column of that type must re-plan to empty: the CREATE TYPE round-trips
    // (introspected from pg_enum), and the column rebinds from Raw("mood") to Enum("mood").
    let _guard = db_lock().lock().await;
    let model = enum_fixture(&["sad", "ok", "happy"]);
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_enums CASCADE")
        .await
        .expect("reset enum fixture");

    squealy_model::publish(&model, &Postgres, &mut connection)
        .await
        .expect("publish enum + column");
    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::default(),
    )
    .await
    .expect("re-plan against the published enum");
    assert!(
        plan.steps.is_empty(),
        "a published enum must re-plan empty, got: {:?}",
        plan.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_enums CASCADE")
        .await
        .expect("clean up enum fixture");
}

fn sequence_fixture() -> DatabaseModel {
    let events = TableModel {
        name: "events".to_owned(),
        comment: None,
        columns: vec![ColumnModel {
            name: "id".to_owned(),
            comment: None,
            ty: SqlType::I64,
            collation: None,
            nullable: false,
            default: None,
            identity: None,
            generated: None,
            on_update: None,
        }],
        primary_key: Some(Constraint {
            prefix_lengths: Vec::new(),
            name: "pk_events".to_owned(),
            columns: vec!["id".to_owned()],
        }),
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
    };
    let standalone = SequenceModel {
        name: "counter".to_owned(),
        data_type: SequenceDataType::Integer,
        start: 100,
        increment: 5,
        min: 100,
        max: 2_147_483_647,
        cache: 1,
        cycle: false,
        owned_by: None,
    };
    let owned = SequenceModel {
        name: "events_id_seq".to_owned(),
        data_type: SequenceDataType::BigInt,
        start: 1,
        increment: 1,
        min: 1,
        max: i64::MAX,
        cache: 1,
        cycle: false,
        owned_by: Some(SequenceOwnedBy {
            table: "events".to_owned(),
            column: "id".to_owned(),
        }),
    };
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("publish_seqs".to_owned()),
            tables: vec![events],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: vec![standalone, owned],
        }],
    }
}

#[tokio::test]
#[ignore]
async fn publishing_sequences_then_replanning_is_empty() {
    // A published standalone sequence (with non-default attributes) and a column-owned sequence must both
    // re-plan to empty: pg_sequence round-trips the concrete attributes, the identity/serial exclusion
    // does not hide a genuinely standalone sequence, and the `OWNED BY` link is recovered.
    let _guard = db_lock().lock().await;
    let model = sequence_fixture();
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_seqs CASCADE")
        .await
        .expect("reset sequence fixture");

    squealy_model::publish(&model, &Postgres, &mut connection)
        .await
        .expect("publish sequences");
    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::default(),
    )
    .await
    .expect("re-plan against the published sequences");
    assert!(
        plan.steps.is_empty(),
        "published sequences must re-plan empty, got: {:?}",
        plan.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_seqs CASCADE")
        .await
        .expect("clean up sequence fixture");
}

#[tokio::test]
#[ignore]
async fn dropping_a_table_and_its_owned_sequence_applies_cleanly() {
    // A sequence OWNED BY a dropped table is cascade-dropped by PostgreSQL. Without the pre-table detach,
    // the plan's explicit DROP SEQUENCE would then fail on a missing object. Publish the fixture, then
    // apply a migration that removes the table and both sequences, and assert PostgreSQL accepts it.
    let _guard = db_lock().lock().await;
    let published = sequence_fixture();
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_seqs CASCADE")
        .await
        .expect("reset sequence fixture");
    squealy_model::publish(&published, &Postgres, &mut connection)
        .await
        .expect("publish sequences");

    // Target: the same schema, emptied of its table and sequences. Diff directly against the known
    // published model (not a whole-database introspection) so only this schema's objects are dropped.
    let target = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("publish_seqs".to_owned()),
            tables: Vec::new(),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
        }],
    };
    let plan =
        squealy_model::plan_models(&target, &published, squealy_model::DiffPolicy::ALLOW_ALL)
            .expect("plan the drop migration");
    squealy_model::apply_plan(&plan, &target, &Postgres, &mut connection)
        .await
        .expect("apply the drop of a table and its owned sequence");

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_seqs CASCADE")
        .await
        .expect("clean up sequence fixture");
}

#[tokio::test]
#[ignore]
async fn identity_column_sequence_is_not_surfaced_as_standalone() {
    // A `GENERATED ... AS IDENTITY` column owns an internal sequence (pg_depend deptype 'i'). Introspection
    // must exclude it, or an identity table would re-plan a spurious `DropSequence` every run.
    let _guard = db_lock().lock().await;
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");
    connection
        .execute_ddl(
            "DROP SCHEMA IF EXISTS publish_seqs CASCADE;\n\
             CREATE SCHEMA publish_seqs;\n\
             CREATE TABLE publish_seqs.widgets (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY)",
        )
        .await
        .expect("create identity table");

    let introspected = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect identity table");
    let schema = introspected
        .schemas
        .iter()
        .find(|schema| schema.name.as_deref() == Some("publish_seqs"))
        .expect("the publish_seqs schema");
    assert!(
        schema.sequences.is_empty(),
        "an identity column's internal sequence must not be introspected as standalone: {:?}",
        schema.sequences
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_seqs CASCADE")
        .await
        .expect("clean up sequence fixture");
}

#[tokio::test]
#[ignore]
async fn changing_enum_labels_is_rejected_for_now() {
    // Enum-label migration (append, remove, or reorder) is not supported yet: a correct migration needs
    // live-schema and whole-plan awareness (live defaults, dependent views/foreign keys, ordering around
    // table changes, PostgreSQL's ADD VALUE-in-transaction rule). Until then, changing an enum's labels
    // is refused loudly rather than emitting SQL PostgreSQL would reject or applying a partial migration.
    let _guard = db_lock().lock().await;
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_enums CASCADE")
        .await
        .expect("reset enum fixture");

    squealy_model::publish(&enum_fixture(&["sad", "ok"]), &Postgres, &mut connection)
        .await
        .expect("publish base enum");

    let changed = enum_fixture(&["sad", "ok", "happy"]);
    let plan = squealy_model::plan_from_database(
        &changed,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("plan the label change (the diff still detects it)");
    let error = squealy_model::apply_plan(&plan, &changed, &Postgres, &mut connection)
        .await
        .expect_err("applying an enum label change must be refused");
    assert!(
        error.to_string().contains("mood"),
        "the refusal must name the enum: {error}"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_enums CASCADE")
        .await
        .expect("clean up enum fixture");
}

#[tokio::test]
#[ignore]
async fn publishing_an_enum_column_with_a_default_replans_empty() {
    // An enum column with a DEFAULT round-trips: PostgreSQL stores the default as `'label'::type`, which
    // introspection converts back to `DefaultValue::Text(label)` — including a label containing `::`
    // (`'a::b'::type`), which lives inside the quotes — so it re-plans empty instead of churning.
    let _guard = db_lock().lock().await;
    let mut model = enum_fixture(&["sad", "ok", "a::b"]);
    model.schemas[0].tables[0].columns[1].default = Some(DefaultValue::Text("a::b".to_owned()));
    let mut connection = Postgres
        .connect(&database_url())
        .await
        .expect("connect to PostgreSQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_enums CASCADE")
        .await
        .expect("reset enum fixture");

    squealy_model::publish(&model, &Postgres, &mut connection)
        .await
        .expect("publish enum + defaulted column");
    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::default(),
    )
    .await
    .expect("re-plan against the defaulted enum column");
    assert!(
        plan.steps.is_empty(),
        "an enum column with a default must re-plan empty, got: {:?}",
        plan.steps
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS publish_enums CASCADE")
        .await
        .expect("clean up enum fixture");
}
