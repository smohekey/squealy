//! Incremental schema-plan rendering and application for SQLite.
//!
//! Two halves: render-level assertions on the DDL [`Sqlite::render_plan`] emits for a diff (native
//! `ALTER TABLE` for the changes SQLite supports in place, a create-copy-drop-rename **rebuild** for
//! the rest), and live in-memory applications that prove a rebuild preserves data — including a
//! foreign-key child's rows, which a naive drop-and-recreate would cascade-delete.

use squealy::{
    ArithmeticOp, CheckModel, ColumnModel, CompareOp, Constraint, CteModel, DatabaseModel,
    DatabasePlan, DatabasePlanStep, DdlExecutor, ExprNode, ForeignKeyAction, ForeignKeyModel,
    IdentityMode, IdentityModel, IndexModel, IndexPrefixLength, ProjectionItem, SchemaBackend,
    SchemaModel, SourceItem, SourceRef, SqlType, TableModel, ViewBody, ViewColumnModel, ViewModel,
    ViewQueryModel, ViewSetOp,
};
use squealy_model::{
    CastColumn, DiffPolicy, PlanApplyOptions, RefactorLog, RefactorOperation, RenameColumn,
    RenameTable, apply_plan, apply_plan_with_options, introspect, plan_from_database,
    plan_from_database_with_refactors, plan_models, plan_models_with_refactors, publish,
};
use squealy_sqlite::{Sqlite, SqliteConnection};
use tokio_rusqlite::Connection as RawConnection;

// --- model builders ---

fn column(name: &str, ty: SqlType, nullable: bool) -> ColumnModel {
    ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty,
        collation: None,
        nullable,
        default: None,
        identity: None,
        generated: None,
        on_update: None,
    }
}

/// An `AUTOINCREMENT` primary-key `id`, matching the shape SQLite introspects back — so a table built
/// from it re-plans to an empty diff.
fn autoincrement_id() -> ColumnModel {
    ColumnModel {
        identity: Some(IdentityModel {
            mode: IdentityMode::AutoIncrement,
        }),
        ..column("id", SqlType::I64, false)
    }
}

fn table(name: &str, columns: Vec<ColumnModel>) -> TableModel {
    TableModel {
        name: name.to_owned(),
        comment: None,
        columns,
        primary_key: None,
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        exclusions: Vec::new(),
    }
}

fn one_table(table: TableModel) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![table],
        }],
    }
}

/// A `widgets(id, active)` table whose introspected shape re-plans to an empty diff (no PK/identity).
fn widget_table() -> TableModel {
    table(
        "widgets",
        vec![
            column("id", SqlType::I64, false),
            column("active", SqlType::I64, false),
        ],
    )
}

/// An `active_widgets` view over `widgets` — `SELECT id FROM widgets WHERE active > 0`.
/// The mutable single-`SELECT` body of a view (these tests only build `SELECT` bodies).
fn view_select_mut(view: &mut ViewModel) -> &mut ViewQueryModel {
    match &mut view.query {
        ViewBody::Select(select) => select,
        ViewBody::Set { .. } | ViewBody::With { .. } => {
            panic!("expected a single-SELECT view body")
        }
    }
}

fn active_widgets_view() -> ViewModel {
    let widget_col = |column: &str| ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: column.to_owned(),
    };
    ViewModel {
        name: "active_widgets".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "id".to_owned(),
            ty: SqlType::I64,
            nullable: false,
        }],
        query: ViewBody::Select(Box::new(ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                internal_alias: None,
                expr: widget_col("id"),
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "widgets".to_owned(),
                alias: "q0_0".to_owned(),
            })),
            joins: Vec::new(),
            filter: Some(ExprNode::Compare {
                op: CompareOp::GreaterThan,
                left: Box::new(widget_col("active")),
                right: Box::new(ExprNode::Literal("0".to_owned())),
            }),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })),
    }
}

/// An `active_widget_ids` view over the `active_widgets` view (view-on-view) — `SELECT id FROM
/// active_widgets`. Used to exercise the transitive-dependent handling around a table rebuild.
fn active_widget_ids_view() -> ViewModel {
    ViewModel {
        name: "active_widget_ids".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "id".to_owned(),
            ty: SqlType::I64,
            nullable: false,
        }],
        query: ViewBody::Select(Box::new(ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                internal_alias: None,
                expr: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "id".to_owned(),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "active_widgets".to_owned(),
                alias: "q0_0".to_owned(),
            })),
            ..ViewQueryModel::default()
        })),
    }
}

/// The `widgets` table plus the `active_widgets` view over it.
fn table_and_view() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widget_table()],
            views: vec![active_widgets_view()],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    }
}

fn render(plan: &DatabasePlan, desired: &DatabaseModel) -> String {
    let mut buffer = Vec::new();
    Sqlite
        .render_plan(plan, desired, &mut buffer)
        .expect("render plan");
    String::from_utf8(buffer).expect("utf-8 DDL")
}

// --- render-level tests (no database) ---

#[test]
fn rebuilds_a_table_for_a_column_type_change() {
    // SQLite has no `ALTER COLUMN`, so a type change becomes a create-copy-drop-rename rebuild.
    let actual = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("amount", SqlType::I64, false),
        ],
    ));
    let desired = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("amount", SqlType::Text, false),
        ],
    ));
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    let sql = render(&plan, &desired);
    assert!(sql.contains("CREATE TABLE \"__squealy_new_t\""), "{sql}");
    // The rebuilt column carries its new TEXT affinity.
    assert!(sql.contains("\"amount\" TEXT"), "{sql}");
    assert!(
        sql.contains("INSERT INTO \"__squealy_new_t\" (\"id\", \"amount\")"),
        "{sql}"
    );
    assert!(
        sql.contains("SELECT \"id\", \"amount\" FROM \"t\""),
        "{sql}"
    );
    assert!(sql.contains("DROP TABLE \"t\""), "{sql}");
    assert!(
        sql.contains("ALTER TABLE \"__squealy_new_t\" RENAME TO \"t\""),
        "{sql}"
    );
}

#[test]
fn adds_a_nullable_column_natively() {
    let actual = one_table(table("t", vec![column("id", SqlType::I64, false)]));
    let desired = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("note", SqlType::Text, true),
        ],
    ));
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    let sql = render(&plan, &desired);
    assert!(
        sql.contains("ALTER TABLE \"t\" ADD COLUMN \"note\" TEXT"),
        "{sql}"
    );
    assert!(
        !sql.contains("__squealy_new_"),
        "no rebuild expected: {sql}"
    );
}

#[test]
fn adds_a_not_null_column_with_a_constant_default_natively() {
    let actual = one_table(table("t", vec![column("id", SqlType::I64, false)]));
    let mut new_column = column("status", SqlType::Text, false);
    new_column.default = Some(squealy::DefaultValue::Text("new".to_owned()));
    let desired = one_table(table(
        "t",
        vec![column("id", SqlType::I64, false), new_column],
    ));
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    let sql = render(&plan, &desired);
    assert!(
        sql.contains("ADD COLUMN \"status\" TEXT NOT NULL DEFAULT 'new'"),
        "{sql}"
    );
    assert!(
        !sql.contains("__squealy_new_"),
        "no rebuild expected: {sql}"
    );
}

#[test]
fn adds_a_collated_column_natively() {
    // SQLite accepts `ALTER TABLE … ADD COLUMN … COLLATE …`, so a collated column is added in place
    // rather than forcing a whole-table rebuild.
    let actual = one_table(table("t", vec![column("id", SqlType::I64, false)]));
    let mut collated = column("name", SqlType::Text, true);
    collated.collation = Some("NOCASE".to_owned());
    let desired = one_table(table(
        "t",
        vec![column("id", SqlType::I64, false), collated],
    ));
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    let sql = render(&plan, &desired);
    assert!(
        sql.contains("ALTER TABLE \"t\" ADD COLUMN \"name\" TEXT COLLATE \"NOCASE\""),
        "{sql}"
    );
    assert!(
        !sql.contains("__squealy_new_"),
        "no rebuild expected: {sql}"
    );
}

#[test]
fn adds_a_unique_constraint_via_rebuild() {
    // A `UNIQUE` constraint is inline-only in SQLite (there is no `ALTER TABLE … ADD CONSTRAINT`), so
    // adding one rebuilds the table.
    let actual = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("slug", SqlType::Text, false),
        ],
    ));
    let mut desired_table = table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("slug", SqlType::Text, false),
        ],
    );
    desired_table.uniques.push(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["slug".to_owned()],
    });
    let desired = one_table(desired_table);
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    let sql = render(&plan, &desired);
    assert!(sql.contains("CREATE TABLE \"__squealy_new_t\""), "{sql}");
    assert!(sql.contains("UNIQUE (\"slug\")"), "{sql}");
    assert!(
        sql.contains("INSERT INTO \"__squealy_new_t\" (\"id\", \"slug\")"),
        "{sql}"
    );
}

#[test]
fn rebuild_recreates_the_target_indexes() {
    // Dropping the old table drops its indexes, so a rebuild recreates the target's index set (its
    // add/drop/alter-index steps are already folded into the target).
    let index = IndexModel {
        name: "idx_t_slug".to_owned(),
        columns: vec!["slug".to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    };
    let mut actual_table = table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("slug", SqlType::I64, false),
        ],
    );
    actual_table.indexes.push(index.clone());
    let mut desired_table = table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("slug", SqlType::Text, false),
        ],
    );
    desired_table.indexes.push(index);
    let plan = plan_models(
        &one_table(desired_table.clone()),
        &one_table(actual_table),
        DiffPolicy::ALLOW_ALL,
    )
    .expect("plan");

    let sql = render(&plan, &one_table(desired_table));
    assert!(sql.contains("CREATE TABLE \"__squealy_new_t\""), "{sql}");
    assert!(
        sql.contains("CREATE INDEX \"idx_t_slug\" ON \"t\" (\"slug\")"),
        "the rebuild recreates the index: {sql}"
    );
}

#[test]
fn renames_a_column_natively_and_records_the_refactor() {
    let actual = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("old", SqlType::Text, false),
        ],
    ));
    let desired = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("new", SqlType::Text, false),
        ],
    ));
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-old-new".to_owned(),
            schema: None,
            table: "t".to_owned(),
            from: "old".to_owned(),
            to: "new".to_owned(),
        })],
    };
    let plan = plan_models_with_refactors(&desired, &actual, &refactors, DiffPolicy::ALLOW_ALL)
        .expect("plan");

    let sql = render(&plan, &desired);
    assert!(
        sql.contains("ALTER TABLE \"t\" RENAME COLUMN \"old\" TO \"new\""),
        "{sql}"
    );
    assert!(
        !sql.contains("__squealy_new_"),
        "a bare rename is native: {sql}"
    );
    assert!(
        sql.contains("CREATE TABLE IF NOT EXISTS \"__squealy_refactors\""),
        "{sql}"
    );
    assert!(
        sql.contains(
            "INSERT OR IGNORE INTO \"__squealy_refactors\" (\"id\") VALUES ('rename-old-new')"
        ),
        "{sql}"
    );
}

#[test]
fn rebuild_copies_a_renamed_column_from_its_old_name() {
    // A rename combined with a type change forces a rebuild; the copy must still map the new column to
    // its old name, and the rename id is recorded.
    let actual = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("old", SqlType::I64, false),
        ],
    ));
    let desired = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("new", SqlType::Text, false),
        ],
    ));
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename".to_owned(),
            schema: None,
            table: "t".to_owned(),
            from: "old".to_owned(),
            to: "new".to_owned(),
        })],
    };
    let plan = plan_models_with_refactors(&desired, &actual, &refactors, DiffPolicy::ALLOW_ALL)
        .expect("plan");

    let sql = render(&plan, &desired);
    assert!(sql.contains("CREATE TABLE \"__squealy_new_t\""), "{sql}");
    assert!(
        sql.contains("INSERT INTO \"__squealy_new_t\" (\"id\", \"new\")"),
        "{sql}"
    );
    assert!(
        sql.contains("SELECT \"id\", \"old\" FROM \"t\""),
        "the copy maps new <- old: {sql}"
    );
    assert!(
        sql.contains("INSERT OR IGNORE INTO \"__squealy_refactors\""),
        "the rename is still recorded through the rebuild: {sql}"
    );
}

#[test]
fn rebuild_errors_when_the_target_table_is_absent_from_desired() {
    let actual = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("amount", SqlType::I64, false),
        ],
    ));
    let desired = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("amount", SqlType::Text, false),
        ],
    ));
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    // A rebuild needs the full target table; rendering against a model without it is an error rather
    // than silently emitting a `CREATE TABLE` with the wrong (or no) columns.
    let mut buffer = Vec::new();
    let error = Sqlite
        .render_plan(&plan, &DatabaseModel::default(), &mut buffer)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

// --- live in-memory application ---

/// A schema-management connection plus a second handle onto the *same* in-memory database, so a test
/// can seed and read table rows directly while the backend applies DDL.
async fn setup() -> (SqliteConnection, RawConnection) {
    let raw = RawConnection::open_in_memory()
        .await
        .expect("open in-memory db");
    raw.call(|conn| conn.execute_batch("PRAGMA foreign_keys = ON"))
        .await
        .expect("enable foreign keys");
    (SqliteConnection::new(raw.clone()), raw)
}

async fn exec(raw: &RawConnection, sql: &'static str) {
    raw.call(move |conn| conn.execute_batch(sql))
        .await
        .expect("execute sql");
}

async fn count(raw: &RawConnection, table: &'static str) -> i64 {
    let sql = format!("SELECT count(*) FROM \"{table}\"");
    raw.call(move |conn| conn.query_row(&sql, [], |row| row.get(0)))
        .await
        .expect("count rows")
}

async fn foreign_keys_enabled(raw: &RawConnection) -> bool {
    raw.call(|conn| conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0)))
        .await
        .expect("read foreign_keys")
        != 0
}

/// The table an index is defined on, per `sqlite_master`.
async fn index_table(raw: &RawConnection, index: &'static str) -> String {
    raw.call(move |conn| {
        conn.query_row(
            "SELECT tbl_name FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [index],
            |row| row.get(0),
        )
    })
    .await
    .expect("index table")
}

/// A general `CAST(x AS ty)` SQLite cannot spell faithfully must be rejected *through the plan path*,
/// not only on a direct `render_create`. The incremental plan path canonicalizes the desired model before
/// rendering, so a fold that narrowed the un-renderable type (e.g. `I128` → the `I64` affinity) would let
/// a lossy `CAST(x AS INTEGER)` render silently. `canonical_sqlite_cast_type` keeps such a cast un-folded
/// so the render-time reject still fires here. git-bug 8fe1530.
#[tokio::test]
async fn an_unfaithful_general_cast_check_is_rejected_through_the_plan_path() {
    let unrenderable = [
        SqlType::I128,
        SqlType::U128,
        SqlType::Decimal {
            precision: 10,
            scale: 2,
        },
    ];
    for ty in unrenderable {
        let (mut connection, _raw) = setup().await;
        let mut widget = table(
            "t",
            vec![autoincrement_id(), column("x", SqlType::I64, false)],
        );
        widget.checks = vec![CheckModel {
            name: "ck_x".to_owned(),
            expression: ExprNode::Cast {
                operand: Box::new(ExprNode::BareColumn {
                    column: "x".to_owned(),
                }),
                ty: ty.clone(),
            },
            validation: None,
            enforcement: None,
        }];
        let result = publish(&one_table(widget), &Sqlite, &mut connection).await;
        assert!(
            result.is_err(),
            "a general cast to {ty:?} must be rejected through the plan path, not silently narrowed"
        );
    }
}

#[tokio::test]
async fn applies_a_native_add_column_and_converges() {
    let (mut connection, raw) = setup().await;
    let v1 = one_table(table(
        "items",
        vec![
            column("id", SqlType::I64, false),
            column("name", SqlType::Text, false),
        ],
    ));
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    exec(
        &raw,
        "INSERT INTO \"items\" (\"id\", \"name\") VALUES (1, 'a'), (2, 'b')",
    )
    .await;

    // v2 adds a nullable column: a native `ALTER TABLE … ADD COLUMN`.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    let items = v2.schemas[0]
        .tables
        .iter_mut()
        .find(|table| table.name == "items")
        .expect("items table");
    items.columns.push(column("note", SqlType::Text, true));

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    assert!(
        !render(&plan, &v2).contains("__squealy_new_"),
        "expected native add"
    );

    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply v2");
    assert_eq!(count(&raw, "items").await, 2, "rows survive an add-column");

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "must converge, got: {:?}",
        replan.steps
    );
}

#[tokio::test]
async fn rebuild_preserves_rows_when_adding_a_unique() {
    let (mut connection, raw) = setup().await;
    let v1 = one_table(table(
        "accounts",
        vec![
            column("id", SqlType::I64, false),
            column("slug", SqlType::Text, false),
        ],
    ));
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    exec(
        &raw,
        "INSERT INTO \"accounts\" (\"id\", \"slug\") VALUES (1, 'a'), (2, 'b')",
    )
    .await;

    // Adding a UNIQUE constraint is inline-only in SQLite, so this rebuilds the table.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    v2.schemas[0].tables[0].uniques.push(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["slug".to_owned()],
    });

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    assert!(
        render(&plan, &v2).contains("__squealy_new_accounts"),
        "expected rebuild"
    );

    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply v2");
    assert_eq!(count(&raw, "accounts").await, 2, "rows survive the rebuild");

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "must converge, got: {:?}",
        replan.steps
    );
}

#[tokio::test]
async fn rebuild_preserves_child_rows_despite_on_delete_cascade() {
    // The load-bearing test for the executor's foreign-key envelope: rebuilding a parent table drops
    // and recreates it, and `DROP TABLE` fires `ON DELETE CASCADE` on child rows while foreign keys are
    // enforced. With enforcement disabled for the batch, the children survive.
    let (mut connection, raw) = setup().await;
    let mut parents = table(
        "parents",
        vec![autoincrement_id(), column("code", SqlType::Text, false)],
    );
    parents.primary_key = Some(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let mut children = table(
        "children",
        vec![autoincrement_id(), column("parent_id", SqlType::I64, false)],
    );
    children.primary_key = Some(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    children.foreign_keys.push(ForeignKeyModel {
        name: String::new(),
        columns: vec!["parent_id".to_owned()],
        references_schema: None,
        references_table: "parents".to_owned(),
        references_columns: vec!["id".to_owned()],
        match_type: None,
        deferrability: None,
        validation: None,
        enforcement: None,
        on_delete: Some(ForeignKeyAction::Cascade),
        on_update: None,
    });
    let v1 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![parents, children],
        }],
    };
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    exec(
        &raw,
        "INSERT INTO \"parents\" (\"id\", \"code\") VALUES (1, 'x')",
    )
    .await;
    exec(
        &raw,
        "INSERT INTO \"children\" (\"id\", \"parent_id\") VALUES (10, 1)",
    )
    .await;

    // Force a rebuild of the parent by adding a UNIQUE constraint on it.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    v2.schemas[0]
        .tables
        .iter_mut()
        .find(|table| table.name == "parents")
        .expect("parents table")
        .uniques
        .push(Constraint {
            prefix_lengths: Vec::new(),
            name: String::new(),
            columns: vec!["code".to_owned()],
        });

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    assert!(
        render(&plan, &v2).contains("__squealy_new_parents"),
        "expected a parent rebuild"
    );

    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply v2");
    assert_eq!(count(&raw, "parents").await, 1, "parent rows survive");
    assert_eq!(
        count(&raw, "children").await,
        1,
        "child rows must survive the parent rebuild (no cascade delete)"
    );

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "must converge, got: {:?}",
        replan.steps
    );
}

#[tokio::test]
async fn foreign_key_check_rejects_a_violating_change() {
    // Turning enforcement off for the batch means a change that leaves inconsistent data must be caught
    // by the executor's `PRAGMA foreign_key_check` and fail, not commit silently. Adding a foreign key
    // whose existing rows have no parent is such a change.
    let (mut connection, raw) = setup().await;
    let mut parents = table("parents", vec![autoincrement_id()]);
    parents.primary_key = Some(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let mut children = table(
        "children",
        vec![autoincrement_id(), column("parent_id", SqlType::I64, false)],
    );
    children.primary_key = Some(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let v1 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![parents, children],
        }],
    };
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    // A child row whose parent_id references a non-existent parent.
    exec(
        &raw,
        "INSERT INTO \"children\" (\"id\", \"parent_id\") VALUES (10, 99)",
    )
    .await;

    // v2 adds a foreign key `children.parent_id -> parents.id`; the existing row (99) violates it.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    v2.schemas[0]
        .tables
        .iter_mut()
        .find(|table| table.name == "children")
        .expect("children table")
        .foreign_keys
        .push(ForeignKeyModel {
            name: String::new(),
            columns: vec!["parent_id".to_owned()],
            references_schema: None,
            references_table: "parents".to_owned(),
            references_columns: vec!["id".to_owned()],
            match_type: None,
            deferrability: None,
            validation: None,
            enforcement: None,
            on_delete: None,
            on_update: None,
        });

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    let error = apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect_err("a foreign-key violation must abort the batch");
    // The batch rolled back, so the orphan row is still there and the FK was not applied.
    assert!(
        error.to_string().contains("foreign key") || error.to_string().contains("ddl"),
        "{error}"
    );
    assert_eq!(count(&raw, "children").await, 1);
}

#[tokio::test]
async fn rebuild_preserves_the_autoincrement_high_water_mark() {
    // A rebuild drops the old table, discarding its `sqlite_sequence` high-water mark. Without carrying
    // it over, AUTOINCREMENT would reuse an id from a row deleted before the rebuild.
    let (mut connection, raw) = setup().await;
    let mut widgets = table(
        "widgets",
        vec![autoincrement_id(), column("v", SqlType::Text, false)],
    );
    widgets.primary_key = Some(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    publish(&one_table(widgets), &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    // Generate ids 1, 2, 3, then delete the top two: the high-water mark is 3 but only id 1 survives.
    exec(
        &raw,
        "INSERT INTO \"widgets\" (\"v\") VALUES ('a'), ('b'), ('c')",
    )
    .await;
    exec(&raw, "DELETE FROM \"widgets\" WHERE \"id\" IN (2, 3)").await;

    // Rebuild by adding a UNIQUE constraint.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    v2.schemas[0].tables[0].uniques.push(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["v".to_owned()],
    });
    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    assert!(
        render(&plan, &v2).contains("__squealy_new_widgets"),
        "expected rebuild"
    );
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply v2");

    // A fresh AUTOINCREMENT insert must not reuse a deleted id (2 or 3): it must be 4.
    exec(&raw, "INSERT INTO \"widgets\" (\"v\") VALUES ('d')").await;
    let new_id: i64 = raw
        .call(|conn| {
            conn.query_row(
                "SELECT \"id\" FROM \"widgets\" WHERE \"v\" = 'd'",
                [],
                |row| row.get(0),
            )
        })
        .await
        .expect("read new id");
    assert_eq!(
        new_id, 4,
        "AUTOINCREMENT must not reuse an id from a deleted row"
    );
}

#[tokio::test]
async fn rebuild_with_concurrent_index_option_does_not_double_create() {
    // SQLite has no concurrent index build (`supports_concurrent_index_creation` is false), so
    // `concurrent_indexes` is ignored and the plan applies transactionally. That matters when a plan
    // both rebuilds a table and adds an index: were the index-add split into a separate phase, the
    // rebuild would recreate the index and the split-out add would create it again. Applying it all in
    // one batch means the rebuild creates the index exactly once.
    let (mut connection, raw) = setup().await;
    let v1 = one_table(table(
        "gauges",
        vec![
            column("id", SqlType::I64, false),
            column("val", SqlType::I64, false),
        ],
    ));
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    exec(
        &raw,
        "INSERT INTO \"gauges\" (\"id\", \"val\") VALUES (1, 10)",
    )
    .await;

    // v2 changes `val`'s type (forces a rebuild) *and* adds an index on it.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    let gauges = &mut v2.schemas[0].tables[0];
    gauges
        .columns
        .iter_mut()
        .find(|column| column.name == "val")
        .expect("val column")
        .ty = SqlType::Text;
    gauges.indexes.push(IndexModel {
        name: "idx_gauges_val".to_owned(),
        columns: vec!["val".to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    });

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    apply_plan_with_options(
        &plan,
        &v2,
        &Sqlite,
        &mut connection,
        PlanApplyOptions {
            concurrent_indexes: true,
            ..PlanApplyOptions::default()
        },
    )
    .await
    .expect("apply with concurrent indexes must not double-create the rebuilt index");
    assert_eq!(count(&raw, "gauges").await, 1, "row survives");

    // The index exists exactly once.
    let index_count: i64 = raw
        .call(|conn| {
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_gauges_val'",
                [],
                |row| row.get(0),
            )
        })
        .await
        .expect("count index");
    assert_eq!(index_count, 1);

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "must converge, got: {:?}",
        replan.steps
    );
}

#[test]
fn rebuild_applies_a_cast_column_expression_in_the_copy() {
    // A `cast-column` refactor supplies a conversion for a column's type change; the rebuild copy must
    // evaluate it, not copy the old value verbatim.
    let actual = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("amount", SqlType::Text, false),
        ],
    ));
    let desired = one_table(table(
        "t",
        vec![
            column("id", SqlType::I64, false),
            column("amount", SqlType::F64, false),
        ],
    ));
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::CastColumn(CastColumn {
            id: "cast-amount".to_owned(),
            schema: None,
            table: "t".to_owned(),
            column: "amount".to_owned(),
            using: "CAST(\"amount\" AS REAL)".to_owned(),
        })],
    };
    let plan = plan_models_with_refactors(&desired, &actual, &refactors, DiffPolicy::ALLOW_ALL)
        .expect("plan");

    let sql = render(&plan, &desired);
    assert!(
        sql.contains("__squealy_new_t"),
        "a type change rebuilds: {sql}"
    );
    assert!(
        sql.contains("SELECT \"id\", CAST(\"amount\" AS REAL) FROM \"t\""),
        "the copy evaluates the cast expression: {sql}"
    );
}

#[tokio::test]
async fn rebuild_evaluates_a_cast_column_conversion() {
    let (mut connection, raw) = setup().await;
    let v1 = one_table(table(
        "money",
        vec![
            column("id", SqlType::I64, false),
            column("amount", SqlType::Text, false),
        ],
    ));
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    exec(
        &raw,
        "INSERT INTO \"money\" (\"id\", \"amount\") VALUES (1, '12.5')",
    )
    .await;

    // v2 changes `amount` from text to a real, with a cast-column conversion.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    v2.schemas[0].tables[0]
        .columns
        .iter_mut()
        .find(|column| column.name == "amount")
        .expect("amount column")
        .ty = SqlType::F64;
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::CastColumn(CastColumn {
            id: "cast-amount".to_owned(),
            schema: None,
            table: "money".to_owned(),
            column: "amount".to_owned(),
            using: "CAST(\"amount\" AS REAL)".to_owned(),
        })],
    };

    let plan =
        plan_from_database_with_refactors(&v2, &refactors, &mut connection, DiffPolicy::ALLOW_ALL)
            .await
            .expect("plan v2");
    assert!(
        render(&plan, &v2).contains("CAST(\"amount\" AS REAL)"),
        "the plan carries the cast"
    );
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply v2");

    // The stored value was converted to a real number, not left as the original text.
    let amount: f64 = raw
        .call(|conn| conn.query_row("SELECT \"amount\" FROM \"money\"", [], |row| row.get(0)))
        .await
        .expect("read amount");
    assert_eq!(amount, 12.5);

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "cast migration must converge, got: {:?}",
        replan.steps
    );
}

#[test]
fn rejects_a_table_comment_change() {
    // SQLite has no table comment and introspection cannot read one back, so a `SetTableComment` step
    // could never converge; it is rejected rather than silently applied as a no-op.
    let actual = one_table(table("t", vec![column("id", SqlType::I64, false)]));
    let mut desired_table = table("t", vec![column("id", SqlType::I64, false)]);
    desired_table.comment = Some("a note".to_owned());
    let desired = one_table(desired_table);
    let plan = plan_models(&desired, &actual, DiffPolicy::ALLOW_ALL).expect("plan");

    let mut buffer = Vec::new();
    let error = Sqlite
        .render_plan(&plan, &desired, &mut buffer)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn rejects_a_target_with_a_duplicate_index_name() {
    // Two indexes sharing a name collide in SQLite's single object namespace; incremental index
    // creation uses `IF NOT EXISTS`, which would silently skip one, so the target namespace is
    // validated up front (as create-from-scratch does).
    let index = |name: &str, column_name: &str| IndexModel {
        name: name.to_owned(),
        columns: vec![column_name.to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    };
    let mut target = table(
        "t",
        vec![
            column("a", SqlType::Text, false),
            column("b", SqlType::Text, false),
        ],
    );
    target.indexes = vec![index("dup", "a"), index("dup", "b")];
    let desired = one_table(target);
    // Any non-empty plan reaches the up-front namespace check; a create-from-empty is the simplest.
    let plan =
        plan_models(&desired, &DatabaseModel::default(), DiffPolicy::ALLOW_ALL).expect("plan");

    let mut buffer = Vec::new();
    let error = Sqlite
        .render_plan(&plan, &desired, &mut buffer)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[tokio::test]
async fn applies_an_index_name_swap_between_tables() {
    // SQLite's index namespace is database-wide, so swapping two tables' index names is only valid if
    // each name is freed before its replacement is created (the per-table drops and adds interleave).
    let index = |name: &str, column_name: &str| IndexModel {
        name: name.to_owned(),
        columns: vec![column_name.to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    };
    let (mut connection, raw) = setup().await;
    let mut left = table("t_left", vec![column("a", SqlType::Text, false)]);
    left.indexes = vec![index("idx_one", "a")];
    let mut right = table("t_right", vec![column("b", SqlType::Text, false)]);
    right.indexes = vec![index("idx_two", "b")];
    let v1 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![left, right],
        }],
    };
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");

    // Swap the two index names between the tables.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    for table in &mut v2.schemas[0].tables {
        for index in &mut table.indexes {
            index.name = if index.name == "idx_one" {
                "idx_two".to_owned()
            } else {
                "idx_one".to_owned()
            };
        }
    }

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("the index-name swap applies");

    assert_eq!(index_table(&raw, "idx_two").await, "t_left");
    assert_eq!(index_table(&raw, "idx_one").await, "t_right");

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "must converge, got: {:?}",
        replan.steps
    );
}

#[tokio::test]
async fn rebuild_replacing_all_columns_preserves_rows() {
    // When every column is dropped and replaced, the rebuild has no column to copy — but the rows must
    // still survive, each taking the new columns' defaults.
    let (mut connection, raw) = setup().await;
    let v1 = one_table(table("t", vec![column("old", SqlType::Text, false)]));
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    exec(
        &raw,
        "INSERT INTO \"t\" (\"old\") VALUES ('a'), ('b'), ('c')",
    )
    .await;

    // Drop `old`, add `new INTEGER DEFAULT 0`: a full rebuild with no carried column.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    let mut new_column = column("new", SqlType::I64, false);
    new_column.default = Some(squealy::DefaultValue::Int(0));
    v2.schemas[0].tables[0].columns = vec![new_column];

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply v2");
    assert_eq!(
        count(&raw, "t").await,
        3,
        "rows must survive replacing every column"
    );
    let defaulted: i64 = raw
        .call(|conn| {
            conn.query_row("SELECT count(*) FROM \"t\" WHERE \"new\" = 0", [], |row| {
                row.get(0)
            })
        })
        .await
        .expect("count defaulted");
    assert_eq!(
        defaulted, 3,
        "each surviving row takes the new column's default"
    );

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "must converge, got: {:?}",
        replan.steps
    );
}

#[tokio::test]
async fn rebuild_preserves_rows_when_a_column_shadows_rowid() {
    // A full-column-replace rebuild carries rows via the hidden rowid; if the new table defines a
    // column literally named `rowid`, the renderer must bind an unshadowed alias so the user column
    // still takes its default rather than the old row ids.
    let (mut connection, raw) = setup().await;
    let v1 = one_table(table("t", vec![column("old", SqlType::Text, false)]));
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");
    exec(&raw, "INSERT INTO \"t\" (\"old\") VALUES ('a'), ('b')").await;

    // Replace the column with one named `rowid` (shadowing the hidden rowid), with a default.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    let mut shadow = column("rowid", SqlType::Text, false);
    shadow.default = Some(squealy::DefaultValue::Text("d".to_owned()));
    v2.schemas[0].tables[0].columns = vec![shadow];

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply v2");
    assert_eq!(
        count(&raw, "t").await,
        2,
        "rows survive despite the rowid-shadowing column"
    );
    // The user `rowid` column got its default, not the old hidden row ids.
    let defaulted: i64 = raw
        .call(|conn| {
            conn.query_row(
                "SELECT count(*) FROM \"t\" WHERE \"rowid\" = 'd'",
                [],
                |row| row.get(0),
            )
        })
        .await
        .expect("count defaulted");
    assert_eq!(defaulted, 2);

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "must converge, got: {:?}",
        replan.steps
    );
}

#[tokio::test]
async fn drops_a_table_before_reusing_its_name_for_an_index() {
    // SQLite's table and index names share one namespace, so an index taking a dropped table's name is
    // valid only if the table is dropped first.
    let index = |name: &str, column_name: &str| IndexModel {
        name: name.to_owned(),
        columns: vec![column_name.to_owned()],
        expressions: Vec::new(),
        include_columns: Vec::new(),
        unique: false,
        method: None,
        directions: Vec::new(),
        nulls: Vec::new(),
        collations: Vec::new(),
        operator_classes: Vec::new(),
        prefix_lengths: Vec::new(),
        predicate: None,
    };
    let (mut connection, raw) = setup().await;
    let v1 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: vec![
                table("keep", vec![column("x", SqlType::Text, false)]),
                table("zzz", vec![column("y", SqlType::Text, false)]),
            ],
        }],
    };
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1");

    // Drop table `zzz`, and add an index *named* `zzz` on the surviving table.
    let mut v2 = introspect(&mut connection).await.expect("introspect");
    v2.schemas[0].tables.retain(|table| table.name == "keep");
    v2.schemas[0].tables[0].indexes = vec![index("zzz", "x")];

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("dropping the table frees its name for the index");
    assert_eq!(index_table(&raw, "zzz").await, "keep");

    let replan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan");
    assert!(
        replan.steps.is_empty(),
        "must converge, got: {:?}",
        replan.steps
    );
}

#[tokio::test]
async fn execute_ddl_restores_the_prior_foreign_keys_setting() {
    // `execute_ddl` disables foreign-key enforcement for the batch (a rebuild's DROP TABLE would
    // otherwise cascade), but it must restore the *prior* setting, not force enforcement on: a handle
    // built via `SqliteConnection::new` that left enforcement off should keep it off.
    let raw = RawConnection::open_in_memory()
        .await
        .expect("open in-memory db");
    // `SqliteConnection::new` does not manage the setting (unlike `connect`); disable it explicitly.
    raw.call(|conn| conn.execute_batch("PRAGMA foreign_keys = OFF"))
        .await
        .expect("disable foreign keys");
    assert!(!foreign_keys_enabled(&raw).await);
    let mut connection = SqliteConnection::new(raw.clone());
    connection
        .execute_ddl("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .await
        .expect("execute ddl");
    assert!(
        !foreign_keys_enabled(&raw).await,
        "execute_ddl must not force foreign_keys on for a handle that left them off"
    );

    // With enforcement on, it stays on after the batch.
    raw.call(|conn| conn.execute_batch("PRAGMA foreign_keys = ON"))
        .await
        .expect("enable foreign keys");
    connection
        .execute_ddl("CREATE TABLE u (id INTEGER PRIMARY KEY)")
        .await
        .expect("execute ddl");
    assert!(
        foreign_keys_enabled(&raw).await,
        "an enabled setting is preserved"
    );
}

#[tokio::test]
async fn introspects_a_published_view_by_name() {
    // A published view is read back so the diff can see it. SQLite stores a view's verbatim `CREATE VIEW`
    // text, so the reverse parser reconstructs its structural body straight from `sqlite_master.sql`
    // (the schema qualifier is suppressed, so the source reads back unqualified). SQLite still cannot
    // type a view's computed output columns, so each carries the sentinel `Bytes` type and only the
    // column *names* are meaningful — the diff compares columns by name and bodies structurally.
    let (mut connection, _raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");

    let actual = introspect(&mut connection).await.expect("introspect");
    let views: Vec<_> = actual
        .schemas
        .iter()
        .flat_map(|schema| &schema.views)
        .collect();
    assert_eq!(
        views.len(),
        1,
        "expected the view to be introspected: {actual:?}"
    );
    assert_eq!(views[0].name, "active_widgets");
    assert_eq!(
        views[0]
            .columns
            .iter()
            .map(|column| (column.name.as_str(), &column.ty))
            .collect::<Vec<_>>(),
        vec![("id", &SqlType::Bytes)],
    );
    // The body reconstructs to the same shape the model rendered — `active_widgets_view`'s body, whose
    // source is already unqualified (`schema: None`), so it round-trips exactly.
    assert_eq!(views[0].query, active_widgets_view().query);
}

#[tokio::test]
async fn replanning_an_unchanged_view_is_not_destructive() {
    // A view whose computed output (`length(name)`) SQLite cannot type must not force a destructive
    // `DropView` on an unchanged replan. The desired view columns canonicalize to the same sentinel type
    // introspection reads back, so they match by name and the diff re-applies the view without a drop —
    // the default `BLOCK_RISKY` policy (which blocks destructive changes) still succeeds.
    let (mut connection, _raw) = setup().await;
    let mut model = one_table(table(
        "people",
        vec![
            column("id", SqlType::I64, false),
            column("name", SqlType::Text, false),
        ],
    ));
    model.schemas[0].views.push(ViewModel {
        name: "name_lengths".to_owned(),
        comment: None,
        columns: vec![
            ViewColumnModel {
                name: "id".to_owned(),
                ty: SqlType::I64,
                nullable: false,
            },
            // A computed output SQLite reports no type for — the case that used to churn a `DropView`.
            ViewColumnModel {
                name: "name_length".to_owned(),
                ty: SqlType::I64,
                nullable: false,
            },
        ],
        query: ViewBody::Select(Box::new(ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: vec![
                ProjectionItem {
                    output_name: "id".to_owned(),
                    internal_alias: None,
                    expr: ExprNode::Column {
                        alias: "q0_0".to_owned(),
                        column: "id".to_owned(),
                    },
                },
                ProjectionItem {
                    output_name: "name_length".to_owned(),
                    internal_alias: None,
                    expr: ExprNode::ScalarFn {
                        func: squealy::ScalarFunc::Length,
                        args: vec![ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "name".to_owned(),
                        }],
                    },
                },
            ],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "people".to_owned(),
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

    publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish table + computed view");

    // Under the default (risk-blocking) policy, the unchanged replan must succeed and carry no drop.
    let plan = plan_from_database(&model, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect("re-plan the unchanged view under the default policy");
    assert!(
        !plan
            .steps
            .iter()
            .any(|step| matches!(step, DatabasePlanStep::DropView { .. })),
        "an unchanged view must not force a destructive DropView: {:?}",
        plan.steps,
    );
}

#[tokio::test]
async fn replanning_a_clause_alias_colliding_with_a_source_column_is_empty() {
    // git-bug 823ae69, the collision shape: `total` names BOTH a computed projection alias and a real
    // `totals.total` source column, and the `ORDER BY total + 0` reference is *nested*.
    //
    // Unlike PostgreSQL/MySQL, this shape does NOT churn on SQLite and never did: SQLite stores the view's
    // verbatim `CREATE VIEW` text, so introspection parses the clause back into the same bare form the
    // desired model authored, and the two compare equal with or without the clause-alias canonicalization.
    // (Verified by disabling the pass — this test still passes; the PostgreSQL live test fails.) So this
    // pins the *negative* result the earlier handoff got wrong by naming SQLite the cleanest repro: the
    // catalog pass must keep SQLite's already-converged shape converged, i.e. never rewrite one side only.
    let (mut connection, _raw) = setup().await;
    let mut model = one_table(table(
        "totals",
        vec![
            column("id", SqlType::I64, false),
            column("total", SqlType::I64, false),
        ],
    ));
    let source_total = ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: "total".to_owned(),
    };
    model.schemas[0].views.push(ViewModel {
        name: "doubled_totals".to_owned(),
        comment: None,
        // A declared column list suppresses each projection's own `AS`, so only the kept internal alias is
        // an in-scope clause name — the shape `internal_alias` exists for (git-bug e1d0724).
        columns: vec![ViewColumnModel {
            name: "total".to_owned(),
            ty: SqlType::I64,
            nullable: false,
        }],
        query: ViewBody::Select(Box::new(ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: Some("total".to_owned()),
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Multiply,
                    left: Box::new(source_total),
                    right: Box::new(ExprNode::Literal("2".to_owned())),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "totals".to_owned(),
                alias: "q0_0".to_owned(),
            })),
            order_by: vec![squealy::OrderItem {
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Add,
                    left: Box::new(ExprNode::BareColumn {
                        column: "total".to_owned(),
                    }),
                    right: Box::new(ExprNode::Literal("0".to_owned())),
                },
                direction: None,
                nulls: None,
            }],
            ..ViewQueryModel::default()
        })),
    });

    publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish table + clause-alias view");

    let plan = plan_from_database(&model, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect("re-plan the unchanged clause-alias view");
    assert!(
        plan.steps.is_empty(),
        "a published clause-alias view must re-plan empty: {:?}",
        plan.steps,
    );
}

/// Counts the live triggers in the database.
async fn trigger_count(raw: &RawConnection) -> i64 {
    raw.call(|conn| {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger'",
            [],
            |row| row.get(0),
        )
    })
    .await
    .expect("count triggers")
}

/// Publishes `widgets` + the `active_widgets` view, then makes the view writeable the only way SQLite
/// allows — an out-of-band `INSTEAD OF` trigger, which squealy does not model.
async fn setup_view_with_trigger() -> (SqliteConnection, RawConnection) {
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");
    exec(
        &raw,
        "CREATE TRIGGER \"active_widgets_insert\" INSTEAD OF INSERT ON \"active_widgets\" \
         BEGIN INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (NEW.\"id\", 1); END",
    )
    .await;
    assert_eq!(trigger_count(&raw).await, 1, "trigger created");
    (connection, raw)
}

/// `table_and_view()` with the view's body changed (same column set) — one `CreateView` step, which the
/// neutral classification calls `Safe` because it is `CREATE OR REPLACE` on PostgreSQL/MySQL.
fn changed_view_model() -> DatabaseModel {
    let mut model = table_and_view();
    view_select_mut(&mut model.schemas[0].views[0]).filter = Some(ExprNode::Compare {
        op: CompareOp::GreaterThan,
        left: Box::new(ExprNode::Column {
            alias: "q0_0".to_owned(),
            column: "id".to_owned(),
        }),
        right: Box::new(ExprNode::Literal("5".to_owned())),
    });
    model
}

/// `table_and_view()` with a CHECK added to the base table. `AddCheck` is classified `Safe`, but SQLite
/// cannot `ALTER ... ADD CONSTRAINT`, so it rebuilds the table — which displaces the *unchanged* view
/// above it. The resulting plan never mentions the view.
fn added_check_model() -> DatabaseModel {
    let mut model = table_and_view();
    model.schemas[0].tables[0].checks.push(CheckModel {
        name: "widgets_active_bool".to_owned(),
        expression: ExprNode::Compare {
            op: CompareOp::GreaterThanOrEquals,
            left: Box::new(ExprNode::BareColumn {
                column: "active".to_owned(),
            }),
            right: Box::new(ExprNode::Literal("0".to_owned())),
        },
        validation: None,
        enforcement: None,
    });
    model
}

#[tokio::test]
async fn changing_a_view_is_refused_when_it_carries_triggers() {
    // git-bug 6a3940a. SQLite has no `CREATE OR REPLACE VIEW`, so this `Safe` CreateView applies as
    // DROP + CREATE and takes the user's INSTEAD OF trigger with it. squealy cannot recreate what it
    // does not model, so the apply-time preflight refuses rather than destroy it silently.
    let (mut connection, raw) = setup_view_with_trigger().await;
    let v2 = changed_view_model();

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect("planning is not what refuses — the trigger is invisible to the diff");
    let error = apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect_err("applying must refuse to wipe the trigger");

    let message = error.to_string();
    assert!(
        message.contains("active_widgets_insert ON active_widgets"),
        "the error must name the trigger it refused to destroy: {message}"
    );
    assert_eq!(
        trigger_count(&raw).await,
        1,
        "the refused apply must leave the trigger intact"
    );
}

#[tokio::test]
async fn rebuilding_a_table_is_refused_when_a_collateral_view_carries_triggers() {
    // The nastier path: the view is UNCHANGED and the plan never mentions it. A `Safe`-classified
    // AddCheck forces a table rebuild, which displaces the view above it — wiping its triggers under
    // the DEFAULT policy. The preflight sees this because it asks the renderer's own drop set.
    let (mut connection, raw) = setup_view_with_trigger().await;
    let v2 = added_check_model();

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect("plan the rebuild");
    assert!(
        !plan.steps.iter().any(|step| matches!(
            step,
            DatabasePlanStep::CreateView { .. } | DatabasePlanStep::DropView { .. }
        )),
        "this plan must not mention the view at all — that is what makes it dangerous: {:?}",
        plan.steps,
    );
    let error = apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect_err("applying must refuse to wipe the collateral view's trigger");

    assert!(
        error.to_string().contains("active_widgets_insert"),
        "the error must name the trigger: {error}"
    );
    assert_eq!(trigger_count(&raw).await, 1, "trigger intact after refusal");
}

#[tokio::test]
async fn a_destructive_policy_lets_the_trigger_wiping_view_change_through() {
    // The opt-in, and the honest cost of taking it: with `allow_destructive` the caller has accepted
    // destructive changes, so the apply proceeds — and the trigger is gone. This documents the loss.
    let (mut connection, raw) = setup_view_with_trigger().await;
    let v2 = changed_view_model();

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the changed view");
    apply_plan_with_options(
        &plan,
        &v2,
        &Sqlite,
        &mut connection,
        PlanApplyOptions {
            policy: DiffPolicy::ALLOW_ALL,
            ..PlanApplyOptions::default()
        },
    )
    .await
    .expect("an allow-destructive apply proceeds");

    assert_eq!(
        trigger_count(&raw).await,
        0,
        "the trigger is destroyed — the opt-in makes that the caller's choice, not a silent loss"
    );
}

#[tokio::test]
async fn a_temp_trigger_on_a_dropped_view_is_also_refused() {
    // A TEMP trigger against a main-schema view lives in `sqlite_temp_master`, NOT `sqlite_master` —
    // but `DROP VIEW` destroys it just the same (verified), so a guard reading only `sqlite_master`
    // would permit exactly the silent loss it exists to prevent.
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");
    exec(
        &raw,
        "CREATE TEMP TRIGGER \"aw_temp\" INSTEAD OF INSERT ON \"active_widgets\" \
         BEGIN INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (NEW.\"id\", 1); END",
    )
    .await;
    let temp_triggers = |raw: RawConnection| async move {
        raw.call(|conn| {
            conn.query_row(
                "SELECT count(*) FROM sqlite_temp_master WHERE type = 'trigger'",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .expect("count temp triggers")
    };
    assert_eq!(temp_triggers(raw.clone()).await, 1, "temp trigger created");
    assert_eq!(
        trigger_count(&raw).await,
        0,
        "and it is invisible in sqlite_master — the point of this test"
    );

    let v2 = changed_view_model();
    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect("plan the changed view");
    let error = apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect_err("applying must refuse to wipe the temp trigger");

    assert!(
        error.to_string().contains("aw_temp"),
        "the error must name the temp trigger: {error}"
    );
    assert_eq!(
        temp_triggers(raw.clone()).await,
        1,
        "temp trigger intact after refusal"
    );
}

#[tokio::test]
async fn a_trigger_on_a_non_ascii_case_variant_view_does_not_block() {
    // SQLite folds identifiers ASCII-only, so `"Ä"` and `"ä"` are DISTINCT views (verified). Folding
    // the guard's names with Unicode `to_lowercase` would conflate them and refuse a plan over a
    // trigger belonging to an unrelated view.
    let (mut connection, raw) = setup().await;
    let mut model = one_table(table("t", vec![column("x", SqlType::I64, false)]));
    let view = |name: &str| ViewModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "x".to_owned(),
            ty: SqlType::I64,
            nullable: false,
        }],
        query: ViewBody::Select(Box::new(ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "x".to_owned(),
                internal_alias: None,
                expr: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "x".to_owned(),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "t".to_owned(),
                alias: "q0_0".to_owned(),
            })),
            ..ViewQueryModel::default()
        })),
    };
    model.schemas[0].views.push(view("Ä"));
    model.schemas[0].views.push(view("ä"));
    publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish two case-variant views");

    // The trigger belongs to `ä`; the plan only changes `Ä`.
    exec(
        &raw,
        "CREATE TRIGGER \"lower_ins\" INSTEAD OF INSERT ON \"ä\" \
         BEGIN INSERT INTO \"t\" (\"x\") VALUES (NEW.\"x\"); END",
    )
    .await;
    let mut v2 = model.clone();
    view_select_mut(&mut v2.schemas[0].views[0]).filter = Some(ExprNode::Compare {
        op: CompareOp::GreaterThan,
        left: Box::new(ExprNode::Column {
            alias: "q0_0".to_owned(),
            column: "x".to_owned(),
        }),
        right: Box::new(ExprNode::Literal("0".to_owned())),
    });

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect("plan the change to the upper-case view");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("a trigger on the distinct lower-case view must not refuse this plan");
    assert_eq!(
        trigger_count(&raw).await,
        1,
        "the other view's trigger stands"
    );
}

#[tokio::test]
async fn a_plan_touching_a_trigger_free_view_is_not_refused() {
    // The preflight must not block ordinary work: no triggers, no refusal.
    let (mut connection, _raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");
    let v2 = changed_view_model();

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect("plan the changed view");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("a view change with no triggers attached applies under the default policy");
}

#[tokio::test]
async fn a_trigger_on_an_undisplaced_view_does_not_block_a_plan() {
    // The preflight must only consider views the plan actually drops. A trigger on a view this plan
    // never displaces is irrelevant — blocking on it would let one trigger freeze the whole schema.
    let (mut connection, raw) = setup().await;
    let mut model = table_and_view();
    model.schemas[0]
        .tables
        .push(table("gadgets", vec![column("id", SqlType::I64, false)]));
    publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish");
    exec(
        &raw,
        "CREATE TRIGGER \"active_widgets_insert\" INSTEAD OF INSERT ON \"active_widgets\" \
         BEGIN INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (NEW.\"id\", 1); END",
    )
    .await;

    // Add a column to the UNRELATED `gadgets` table: no view is displaced.
    let mut v2 = model.clone();
    v2.schemas[0].tables[1]
        .columns
        .push(column("label", SqlType::Text, true));

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect("plan the unrelated column add");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("a plan that displaces no view must not be refused");
    assert_eq!(trigger_count(&raw).await, 1, "trigger untouched");
}

#[tokio::test]
async fn removing_a_view_drops_it() {
    // Removing a view from the desired model must drop the live view. Before views were introspected the
    // view was invisible to the diff, so no `DropView` was emitted and the stale view lingered (and could
    // block a later object reusing its name).
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");

    // The rendered `CREATE VIEW` is valid SQLite: the view exists and filters to the active rows.
    exec(
        &raw,
        "INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (1, 1), (2, 0)",
    )
    .await;
    assert_eq!(
        count(&raw, "active_widgets").await,
        1,
        "view filters active rows"
    );

    let table_only = one_table(widget_table());
    let plan = plan_from_database(&table_only, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan table-only");
    assert!(
        plan.steps.iter().any(|step| matches!(
            step,
            DatabasePlanStep::DropView { view, .. } if view.name == "active_widgets"
        )),
        "expected a DropView for the removed view, got: {:?}",
        plan.steps,
    );

    apply_plan(&plan, &table_only, &Sqlite, &mut connection)
        .await
        .expect("apply the drop");
    let actual = introspect(&mut connection)
        .await
        .expect("introspect after drop");
    assert!(
        actual.schemas.iter().all(|schema| schema.views.is_empty()),
        "the view must be gone after the drop: {actual:?}",
    );
}

#[tokio::test]
async fn rebuilding_a_table_under_an_existing_view_succeeds() {
    // A rebuild drops the old table and renames the new one into place; a live view over that table is
    // reparsed at the rename. This must not error, and the view must still resolve afterward.
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");
    exec(
        &raw,
        "INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (1, 1), (2, 0)",
    )
    .await;

    // v2 adds a UNIQUE (inline-only in SQLite → forces a create-copy-drop-rename rebuild of widgets),
    // with the active_widgets view over widgets still present.
    let mut widgets = widget_table();
    widgets.uniques.push(Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widgets],
            views: vec![active_widgets_view()],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    assert!(
        render(&plan, &v2).contains("__squealy_new_widgets"),
        "expected a rebuild of widgets",
    );

    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the rebuild under an existing view");
    assert_eq!(
        count(&raw, "active_widgets").await,
        1,
        "the view still resolves and filters after the rebuild",
    );
}

#[tokio::test]
async fn rebuilding_a_table_under_a_chained_view_succeeds() {
    // A rebuild of `widgets` reparses views over it. `active_widgets` selects from `widgets`, and
    // `active_widget_ids` selects from `active_widgets` (a view-on-view chain). Both are unchanged, so
    // the diff emits no view step. The pre-pass must drop the WHOLE transitive closure over the rebuilt
    // table — not just the direct dependent — and recreate it, or SQLite errors reparsing the surviving
    // indirect dependent while its (dropped) source view is momentarily absent mid-rebuild.
    let (mut connection, raw) = setup().await;
    let with_chain = |uniques: Vec<Constraint>| {
        let mut widgets = widget_table();
        widgets.uniques = uniques;
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![widgets],
                views: vec![active_widgets_view(), active_widget_ids_view()],
                enums: Vec::new(),
                sequences: Vec::new(),
                domains: Vec::new(),
            }],
        }
    };
    publish(&with_chain(Vec::new()), &Sqlite, &mut connection)
        .await
        .expect("publish table + chained views");
    exec(
        &raw,
        "INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (1, 1), (2, 0)",
    )
    .await;

    // Adding a UNIQUE forces a create-copy-drop-rename rebuild of widgets under the unchanged chain.
    let v2 = with_chain(vec![Constraint {
        prefix_lengths: Vec::new(),
        name: String::new(),
        columns: vec!["id".to_owned()],
    }]);
    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan v2");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the rebuild under a chained view");
    assert_eq!(
        count(&raw, "active_widget_ids").await,
        1,
        "the chained view still resolves after the rebuild",
    );
}

#[tokio::test]
async fn replacing_a_table_with_a_same_named_view_succeeds() {
    // A plan that drops a table and creates a view of the same name must free the table name (via
    // `DROP TABLE`) before the view pre-pass runs `DROP VIEW IF EXISTS <name>` — SQLite errors ("use
    // DROP TABLE …") if a table still owns the name. The view pre-pass therefore runs after table drops.
    let (mut connection, raw) = setup().await;
    let v1 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![
                widget_table(),
                table("summary", vec![column("id", SqlType::I64, false)]),
            ],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1 (two tables)");
    exec(
        &raw,
        "INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (1, 1), (2, 0)",
    )
    .await;

    // v2 removes the `summary` table and adds a `summary` view (same name) over `widgets`.
    let mut summary_view = active_widgets_view();
    summary_view.name = "summary".to_owned();
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widget_table()],
            views: vec![summary_view],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the table→view replacement");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the table→view replacement");
    assert_eq!(
        count(&raw, "summary").await,
        1,
        "the same-named view resolves after replacing the table",
    );
}

#[tokio::test]
async fn replacing_a_view_with_a_same_named_table_succeeds() {
    // The symmetric transition: a view is dropped and a table of the same name is created. The view
    // pre-pass drops the view (freeing the name) before the main pass creates the table.
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish widgets + active_widgets view");

    // v2 removes the active_widgets view and adds a table of the same name.
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![
                widget_table(),
                table("active_widgets", vec![column("id", SqlType::I64, false)]),
            ],
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };

    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the view→table replacement");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the view→table replacement");
    exec(&raw, "INSERT INTO \"active_widgets\" (\"id\") VALUES (7)").await;
    assert_eq!(
        count(&raw, "active_widgets").await,
        1,
        "the same-named table exists and accepts rows after replacing the view",
    );
}

#[tokio::test]
async fn renaming_a_table_and_reusing_its_name_for_a_view_succeeds() {
    // A refactor renames table `x`→`y`, and a new view named `x` reuses the freed name. The view
    // pre-pass must not emit `DROP VIEW IF EXISTS "x"` while `x` is still a table — the rename frees the
    // name later, in the main pass, and SQLite rejects `DROP VIEW` on a table ("use DROP TABLE").
    let (mut connection, raw) = setup().await;
    let v1 = one_table(table(
        "x",
        vec![
            column("id", SqlType::I64, false),
            column("active", SqlType::I64, false),
        ],
    ));
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1 (table x)");
    exec(
        &raw,
        "INSERT INTO \"x\" (\"id\", \"active\") VALUES (1, 1), (2, 0)",
    )
    .await;

    // v2 renames x→y and adds a view `x` over the renamed table `y`.
    let mut view_x = active_widgets_view();
    view_x.name = "x".to_owned();
    view_select_mut(&mut view_x).from = Some(SourceItem::Named(SourceRef {
        schema: None,
        name: "y".to_owned(),
        alias: "q0_0".to_owned(),
    }));
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![table(
                "y",
                vec![
                    column("id", SqlType::I64, false),
                    column("active", SqlType::I64, false),
                ],
            )],
            views: vec![view_x],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameTable(RenameTable {
            id: "rename-x-y".to_owned(),
            schema: None,
            from: "x".to_owned(),
            to: "y".to_owned(),
        })],
    };

    let plan =
        plan_from_database_with_refactors(&v2, &refactors, &mut connection, DiffPolicy::ALLOW_ALL)
            .await
            .expect("plan the rename + same-named view");
    assert!(
        !render(&plan, &v2).contains("DROP VIEW IF EXISTS \"x\""),
        "must not pre-drop a name a table still owns: {}",
        render(&plan, &v2),
    );

    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the rename + same-named view");
    assert_eq!(
        count(&raw, "x").await,
        1,
        "the new view x (over renamed table y) resolves and filters",
    );
}

#[tokio::test]
async fn a_view_column_set_change_is_a_blocked_destructive_change() {
    // Renaming a view's output column changes its column set. The diff sees this (via the column names it
    // reads back) and emits a destructive `DropView` + re-create — so the default `BLOCK_RISKY` policy
    // blocks it, matching how a table-column drop (or a PostgreSQL view column change) is gated.
    let (mut connection, _raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");

    // v2 renames the view's `id` output column to `widget_id` (same body, different column set).
    let mut renamed = active_widgets_view();
    renamed.columns[0].name = "widget_id".to_owned();
    view_select_mut(&mut renamed).projection[0].output_name = "widget_id".to_owned();
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widget_table()],
            views: vec![renamed],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };

    // The destructive drop blocks the plan under the default policy.
    plan_from_database(&v2, &mut connection, DiffPolicy::BLOCK_RISKY)
        .await
        .expect_err("a view column-set change must be blocked under BLOCK_RISKY");

    // With destructive changes allowed, the plan drops and recreates the view.
    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan under ALLOW_ALL");
    assert!(
        plan.steps.iter().any(|step| matches!(
            step,
            DatabasePlanStep::DropView { view, .. } if view.name == "active_widgets"
        )),
        "expected a DropView for the column-set change: {:?}",
        plan.steps,
    );
}

#[tokio::test]
async fn renaming_a_table_and_reusing_its_name_for_a_view_case_insensitively_succeeds() {
    // Same as the rename+view case, but the new view reuses the table name with different casing
    // (`Thing` renamed away, view `thing` created). SQLite folds identifiers, so the pre-drop skip must
    // fold too — otherwise `DROP VIEW IF EXISTS "thing"` would hit the still-present table `Thing`.
    let (mut connection, raw) = setup().await;
    let v1 = one_table(table(
        "Thing",
        vec![
            column("id", SqlType::I64, false),
            column("active", SqlType::I64, false),
        ],
    ));
    publish(&v1, &Sqlite, &mut connection)
        .await
        .expect("publish v1 (table Thing)");
    exec(
        &raw,
        "INSERT INTO \"Thing\" (\"id\", \"active\") VALUES (1, 1), (2, 0)",
    )
    .await;

    let mut view_thing = active_widgets_view();
    view_thing.name = "thing".to_owned();
    view_select_mut(&mut view_thing).from = Some(SourceItem::Named(SourceRef {
        schema: None,
        name: "renamed".to_owned(),
        alias: "q0_0".to_owned(),
    }));
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![table(
                "renamed",
                vec![
                    column("id", SqlType::I64, false),
                    column("active", SqlType::I64, false),
                ],
            )],
            views: vec![view_thing],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameTable(RenameTable {
            id: "rename-thing".to_owned(),
            schema: None,
            from: "Thing".to_owned(),
            to: "renamed".to_owned(),
        })],
    };

    let plan =
        plan_from_database_with_refactors(&v2, &refactors, &mut connection, DiffPolicy::ALLOW_ALL)
            .await
            .expect("plan the case-differing rename + view");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the case-differing rename + view");
    assert_eq!(
        count(&raw, "thing").await,
        1,
        "the view resolves after the case-differing rename",
    );
}

#[tokio::test]
async fn introspects_and_drops_a_view_whose_table_was_removed() {
    // A view left dangling by an out-of-band `DROP TABLE` makes `PRAGMA table_info` error. Introspection
    // must still succeed (reporting the view name-only) so a plan can drop the broken view, instead of
    // failing outright and stranding the whole database.
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");
    exec(&raw, "DROP TABLE \"widgets\"").await;

    let actual = introspect(&mut connection)
        .await
        .expect("introspection must not fail on a broken view");
    let views: Vec<_> = actual
        .schemas
        .iter()
        .flat_map(|schema| &schema.views)
        .collect();
    assert_eq!(
        views.len(),
        1,
        "the broken view is still reported: {actual:?}"
    );
    assert_eq!(views[0].name, "active_widgets");
    assert!(
        views[0].columns.is_empty(),
        "a body-unanalyzable view is name-only: {:?}",
        views[0].columns,
    );

    // A model that no longer wants the view drops it, and the drop applies.
    let empty = DatabaseModel::default();
    let plan = plan_from_database(&empty, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan dropping the broken view");
    assert!(
        plan.steps.iter().any(|step| matches!(
            step,
            DatabasePlanStep::DropView { view, .. } if view.name == "active_widgets"
        )),
        "expected a DropView for the broken view: {:?}",
        plan.steps,
    );
    apply_plan(&plan, &empty, &Sqlite, &mut connection)
        .await
        .expect("apply the drop of the broken view");
    let after = introspect(&mut connection)
        .await
        .expect("introspect after drop");
    assert!(
        after.schemas.iter().all(|schema| schema.views.is_empty()),
        "the broken view must be gone: {after:?}",
    );
}

#[tokio::test]
async fn a_recursive_cte_view_is_valid_sqlite() {
    // A `WITH RECURSIVE` view body must render DDL SQLite actually accepts — not just SQL the pinned
    // parser accepts. SQLite's recursive-CTE grammar rejects a *parenthesized* recursive arm (`(SELECT …)`
    // or `SELECT * FROM (…)`) with a syntax error, so the renderer emits the arms **bare**
    // (`<anchor> UNION ALL <recursive>`). This publishes a counter CTE view and queries it end to end, so
    // a regression back to parenthesized arms (which the sqlparser-based round-trip harness would NOT
    // catch) fails here. The CTE has a declared column list (`n`) whose recursive arm references `counter`.
    let (mut connection, raw) = setup().await;
    let counter_col = |column: &str| ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: column.to_owned(),
    };
    let view = ViewModel {
        name: "v_counter".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "n".to_owned(),
            ty: SqlType::I64,
            nullable: false,
        }],
        query: ViewBody::With {
            recursive: true,
            ctes: vec![CteModel {
                name: "counter".to_owned(),
                columns: vec!["n".to_owned()],
                body: ViewBody::Set {
                    op: ViewSetOp::Union,
                    all: true,
                    // Anchor `SELECT 1` (no FROM).
                    left: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                        projection: vec![ProjectionItem {
                            output_name: "n".to_owned(),
                            internal_alias: None,
                            expr: ExprNode::Literal("1".to_owned()),
                        }],
                        ..ViewQueryModel::default()
                    }))),
                    // Recursive arm `SELECT counter.n + 1 FROM counter WHERE counter.n < 5`.
                    right: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                        projection: vec![ProjectionItem {
                            output_name: "n".to_owned(),
                            internal_alias: None,
                            expr: ExprNode::Binary {
                                op: ArithmeticOp::Add,
                                left: Box::new(counter_col("n")),
                                right: Box::new(ExprNode::Literal("1".to_owned())),
                            },
                        }],
                        from: Some(SourceItem::Named(SourceRef {
                            schema: None,
                            name: "counter".to_owned(),
                            alias: "q0_0".to_owned(),
                        })),
                        filter: Some(ExprNode::Compare {
                            op: CompareOp::LessThan,
                            left: Box::new(counter_col("n")),
                            right: Box::new(ExprNode::Literal("5".to_owned())),
                        }),
                        ..ViewQueryModel::default()
                    }))),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                },
            }],
            body: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "n".to_owned(),
                    internal_alias: None,
                    expr: counter_col("n"),
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: "counter".to_owned(),
                    alias: "q0_0".to_owned(),
                })),
                ..ViewQueryModel::default()
            }))),
        },
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: Vec::new(),
            views: vec![view],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };

    publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish a recursive-CTE view (rendered DDL must be valid SQLite)");

    // The view resolves and produces the counter rows 1..=5.
    assert_eq!(
        count(&raw, "v_counter").await,
        5,
        "the recursive CTE view must yield 5 counter rows",
    );
}

#[test]
fn a_scoped_recursive_cte_arm_is_rejected_on_sqlite() {
    // SQLite's recursive-CTE grammar forbids a *parenthesized* recursive arm, so an arm carrying its own
    // ORDER BY/LIMIT/OFFSET (which can only be scoped by parenthesizing it) has no valid rendering — the
    // renderer rejects it rather than emit DDL SQLite cannot run. (PostgreSQL/MySQL render it parenthesized.)
    let counter_col = |column: &str| ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: column.to_owned(),
    };
    let view = ViewModel {
        name: "v_counter".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "n".to_owned(),
            ty: SqlType::I64,
            nullable: false,
        }],
        query: ViewBody::With {
            recursive: true,
            ctes: vec![CteModel {
                name: "counter".to_owned(),
                columns: vec!["n".to_owned()],
                body: ViewBody::Set {
                    op: ViewSetOp::Union,
                    all: true,
                    // A *scoped* anchor: `SELECT 1 LIMIT 1`. Bare, its LIMIT would bind to the whole UNION;
                    // parenthesizing it is the only way to scope it, which SQLite forbids for a recursive arm.
                    left: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                        projection: vec![ProjectionItem {
                            output_name: "n".to_owned(),
                            internal_alias: None,
                            expr: ExprNode::Literal("1".to_owned()),
                        }],
                        limit: Some(1),
                        ..ViewQueryModel::default()
                    }))),
                    right: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                        projection: vec![ProjectionItem {
                            output_name: "n".to_owned(),
                            internal_alias: None,
                            expr: ExprNode::Binary {
                                op: ArithmeticOp::Add,
                                left: Box::new(counter_col("n")),
                                right: Box::new(ExprNode::Literal("1".to_owned())),
                            },
                        }],
                        from: Some(SourceItem::Named(SourceRef {
                            schema: None,
                            name: "counter".to_owned(),
                            alias: "q0_0".to_owned(),
                        })),
                        ..ViewQueryModel::default()
                    }))),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                },
            }],
            body: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "n".to_owned(),
                    internal_alias: None,
                    expr: counter_col("n"),
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: "counter".to_owned(),
                    alias: "q0_0".to_owned(),
                })),
                ..ViewQueryModel::default()
            }))),
        },
    };
    let desired = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: Vec::new(),
            views: vec![view],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    let plan =
        plan_models(&desired, &DatabaseModel::default(), DiffPolicy::ALLOW_ALL).expect("plan");
    let mut buffer = Vec::new();
    let error = Sqlite.render_plan(&plan, &desired, &mut buffer).expect_err(
        "SQLite must reject a scoped recursive CTE arm — it has no valid rendering there",
    );
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[tokio::test]
async fn a_recursive_cte_with_a_nested_with_prelude_is_valid_sqlite() {
    // A recursive CTE whose body carries its OWN leading `WITH` prelude — `WITH RECURSIVE counter(n) AS
    // (WITH seed(s) AS (SELECT 1) SELECT s FROM seed UNION ALL SELECT counter.n + 1 FROM counter WHERE …)`.
    // The recursive `Set` sits behind a `ViewBody::With`, so the renderer must recurse through the leading
    // prelude to reach it and still emit **bare** arms — else SQLite wraps the recursive reference in
    // `SELECT * FROM (…)` and the view fails to create. This publishes the view and queries it end to end.
    let (mut connection, raw) = setup().await;
    let counter_col = |column: &str| ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: column.to_owned(),
    };
    // Anchor: `SELECT q1_0.s AS n FROM seed AS q1_0` (reads the inner `seed` CTE).
    let anchor = ViewBody::Select(Box::new(ViewQueryModel {
        projection: vec![ProjectionItem {
            output_name: "n".to_owned(),
            internal_alias: None,
            expr: ExprNode::Column {
                alias: "q1_0".to_owned(),
                column: "s".to_owned(),
            },
        }],
        from: Some(SourceItem::Named(SourceRef {
            schema: None,
            name: "seed".to_owned(),
            alias: "q1_0".to_owned(),
        })),
        ..ViewQueryModel::default()
    }));
    // Recursive arm: `SELECT counter.n + 1 AS n FROM counter AS q0_0 WHERE counter.n < 5`.
    let recursive = ViewBody::Select(Box::new(ViewQueryModel {
        projection: vec![ProjectionItem {
            output_name: "n".to_owned(),
            internal_alias: None,
            expr: ExprNode::Binary {
                op: ArithmeticOp::Add,
                left: Box::new(counter_col("n")),
                right: Box::new(ExprNode::Literal("1".to_owned())),
            },
        }],
        from: Some(SourceItem::Named(SourceRef {
            schema: None,
            name: "counter".to_owned(),
            alias: "q0_0".to_owned(),
        })),
        filter: Some(ExprNode::Compare {
            op: CompareOp::LessThan,
            left: Box::new(counter_col("n")),
            right: Box::new(ExprNode::Literal("5".to_owned())),
        }),
        ..ViewQueryModel::default()
    }));
    let view = ViewModel {
        name: "v_counter".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "n".to_owned(),
            ty: SqlType::I64,
            nullable: false,
        }],
        query: ViewBody::With {
            recursive: true,
            ctes: vec![CteModel {
                name: "counter".to_owned(),
                columns: vec!["n".to_owned()],
                // The recursive CTE's body is itself a `WITH seed AS (…)` wrapping the recursive `Set`.
                body: ViewBody::With {
                    recursive: false,
                    ctes: vec![CteModel {
                        name: "seed".to_owned(),
                        columns: vec!["s".to_owned()],
                        body: ViewBody::Select(Box::new(ViewQueryModel {
                            projection: vec![ProjectionItem {
                                output_name: "s".to_owned(),
                                internal_alias: None,
                                expr: ExprNode::Literal("1".to_owned()),
                            }],
                            ..ViewQueryModel::default()
                        })),
                    }],
                    body: Box::new(ViewBody::Set {
                        op: ViewSetOp::Union,
                        all: true,
                        left: Box::new(anchor),
                        right: Box::new(recursive),
                        order_by: Vec::new(),
                        limit: None,
                        offset: None,
                    }),
                },
            }],
            body: Box::new(ViewBody::Select(Box::new(ViewQueryModel {
                projection: vec![ProjectionItem {
                    output_name: "n".to_owned(),
                    internal_alias: None,
                    expr: counter_col("n"),
                }],
                from: Some(SourceItem::Named(SourceRef {
                    schema: None,
                    name: "counter".to_owned(),
                    alias: "q0_0".to_owned(),
                })),
                ..ViewQueryModel::default()
            }))),
        },
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: Vec::new(),
            views: vec![view],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };

    publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish a nested-WITH recursive-CTE view (rendered DDL must be valid SQLite)");

    assert_eq!(
        count(&raw, "v_counter").await,
        5,
        "the nested-WITH recursive CTE view must yield 5 counter rows",
    );
}

#[tokio::test]
async fn replanning_a_view_with_an_ilike_filter_is_empty() {
    // SQLite has no `ILIKE`: a view whose body filters with `ILIKE` (`case_insensitive: true`) renders as
    // plain `LIKE`, which the reverse parser reads back as `case_insensitive: false`. Now that view bodies
    // compare structurally, `canonical_view_body` must fold the flag on both sides (like it does for
    // checks/index predicates), or the reconstructed body churns a perpetual `CreateView`.
    let (mut connection, _raw) = setup().await;
    let tbl = table(
        "widgets",
        vec![
            column("id", SqlType::I64, false),
            column("name", SqlType::Text, false),
        ],
    );
    let view = ViewModel {
        name: "named_widgets".to_owned(),
        comment: None,
        columns: vec![ViewColumnModel {
            name: "name".to_owned(),
            ty: SqlType::Text,
            nullable: false,
        }],
        query: ViewBody::Select(Box::new(ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "name".to_owned(),
                internal_alias: None,
                expr: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "name".to_owned(),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "widgets".to_owned(),
                alias: "q0_0".to_owned(),
            })),
            filter: Some(ExprNode::Like {
                case_insensitive: true,
                negated: false,
                operand: Box::new(ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "name".to_owned(),
                }),
                pattern: Box::new(ExprNode::Literal("'a%'".to_owned())),
            }),
            ..ViewQueryModel::default()
        })),
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![tbl],
            views: vec![view],
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
        }],
    };
    publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish ilike view");
    let plan = plan_from_database(&model, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("re-plan ilike view");
    assert!(plan.steps.is_empty(), "ILIKE view churn: {:?}", plan.steps);
}

#[test]
fn sqlite_rejects_a_constraint_column_prefix_length() {
    // SQLite has no `col(n)` constraint prefix. A constraint change rebuilds the table through
    // `write_create_table`, so the reject must live there — not silently drop the `(n)` and emit a
    // full-column constraint that would never round-trip.
    let mut items = table("items", vec![column("name", SqlType::Text, false)]);
    items.uniques = vec![Constraint {
        name: "uq_items_name".to_owned(),
        columns: vec!["name".to_owned()],
        prefix_lengths: vec![IndexPrefixLength {
            position: 0,
            length: 10,
        }],
    }];
    let error = Sqlite
        .render_create(&one_table(items), &mut Vec::new())
        .expect_err("a prefix constraint must be rejected");
    assert!(
        error.to_string().contains("prefix length"),
        "unexpected error: {error}"
    );
}
