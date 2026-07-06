//! Incremental schema-plan rendering and application for SQLite.
//!
//! Two halves: render-level assertions on the DDL [`Sqlite::render_plan`] emits for a diff (native
//! `ALTER TABLE` for the changes SQLite supports in place, a create-copy-drop-rename **rebuild** for
//! the rest), and live in-memory applications that prove a rebuild preserves data — including a
//! foreign-key child's rows, which a naive drop-and-recreate would cascade-delete.

use squealy::{
    ColumnModel, CompareOp, Constraint, DatabaseModel, DatabasePlan, DatabasePlanStep, DdlExecutor,
    ExprNode, ForeignKeyAction, ForeignKeyModel, IdentityMode, IdentityModel, IndexModel,
    ProjectionItem, SchemaBackend, SchemaModel, SourceRef, SqlType, TableModel, ViewColumnModel,
    ViewModel, ViewQueryModel,
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
    }
}

fn one_table(table: TableModel) -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
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
        query: ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                expr: widget_col("id"),
            }],
            from: Some(SourceRef {
                schema: None,
                name: "widgets".to_owned(),
                alias: "q0_0".to_owned(),
            }),
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
        },
    }
}

/// The `widgets` table plus the `active_widgets` view over it.
fn table_and_view() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widget_table()],
            views: vec![active_widgets_view()],
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

/// Whether a trigger of the given name exists, per `sqlite_master`.
async fn trigger_exists(raw: &RawConnection, name: &'static str) -> bool {
    raw.call(move |conn| {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
    })
    .await
    .expect("trigger count")
        != 0
}

/// The stored `CREATE VIEW` text of a view, per `sqlite_master` (verbatim, so it can be re-emitted to
/// reproduce a byte-identical churn re-apply the way squealy's renderer does).
async fn view_sql(raw: &RawConnection, name: &'static str) -> String {
    raw.call(move |conn| {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = ?1",
            [name],
            |row| row.get::<_, String>(0),
        )
    })
    .await
    .expect("view sql")
}

/// Whether a view of the given name exists, per `sqlite_master`.
async fn view_exists(raw: &RawConnection, name: &'static str) -> bool {
    raw.call(move |conn| {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'view' AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
    })
    .await
    .expect("view count")
        != 0
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
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let mut children = table(
        "children",
        vec![autoincrement_id(), column("parent_id", SqlType::I64, false)],
    );
    children.primary_key = Some(Constraint {
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
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let mut children = table(
        "children",
        vec![autoincrement_id(), column("parent_id", SqlType::I64, false)],
    );
    children.primary_key = Some(Constraint {
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let v1 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
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
        predicate: None,
    };
    let (mut connection, raw) = setup().await;
    let v1 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
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
    // A published view is read back so the diff can see it. SQLite can't recover a view's structural body
    // (empty projection — the "body unknown" marker) or type its columns, so each column carries the
    // sentinel `Bytes` type and only the column *names* are meaningful.
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
    assert!(
        views[0].query.projection.is_empty(),
        "an introspected view is body-unknown: {:?}",
        views[0].query,
    );
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
        query: ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: vec![
                ProjectionItem {
                    output_name: "id".to_owned(),
                    expr: ExprNode::Column {
                        alias: "q0_0".to_owned(),
                        column: "id".to_owned(),
                    },
                },
                ProjectionItem {
                    output_name: "name_length".to_owned(),
                    expr: ExprNode::ScalarFn {
                        func: squealy::ScalarFunc::Length,
                        args: vec![ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "name".to_owned(),
                        }],
                    },
                },
            ],
            from: Some(SourceRef {
                schema: None,
                name: "people".to_owned(),
                alias: "q0_0".to_owned(),
            }),
            joins: Vec::new(),
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        },
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
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widgets],
            views: vec![active_widgets_view()],
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
async fn republishing_a_view_preserves_its_instead_of_trigger() {
    // squealy has no trigger model, and a body-unknown (introspected) view re-emits `CreateView` every
    // publish. SQLite has no `CREATE OR REPLACE VIEW`, so that renders `DROP VIEW … ; CREATE VIEW …`, and
    // `DROP VIEW` silently drops the view's `INSTEAD OF` triggers — the mechanism that makes a view
    // writeable. The executor captures triggers before the batch and replays any it drops out from under
    // a surviving object, so a no-op re-publish must leave a user-attached trigger (and writes through the
    // view) intact.
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");

    // A user attaches an INSTEAD OF INSERT trigger out of band so `active_widgets` is writeable.
    exec(
        &raw,
        "CREATE TRIGGER \"active_widgets_insert\" INSTEAD OF INSERT ON \"active_widgets\" \
         BEGIN INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (NEW.\"id\", 1); END",
    )
    .await;
    exec(&raw, "INSERT INTO \"active_widgets\" (\"id\") VALUES (1)").await;
    assert_eq!(count(&raw, "widgets").await, 1, "the trigger wrote through");

    // Re-planning the same model against the live DB re-emits `CreateView` for the body-unknown view
    // (the churn this bug documents), which SQLite renders as DROP VIEW + CREATE VIEW.
    let plan = plan_from_database(&table_and_view(), &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the unchanged re-publish");
    assert!(
        plan.steps.iter().any(|step| matches!(
            step,
            DatabasePlanStep::CreateView { view, .. } if view.name == "active_widgets"
        )),
        "expected the view to re-apply (drop + create), got: {:?}",
        plan.steps,
    );
    apply_plan(&plan, &table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("re-apply the unchanged model");

    assert!(
        trigger_exists(&raw, "active_widgets_insert").await,
        "the INSTEAD OF trigger must survive the view re-apply",
    );
    // The trigger still fires, so writes through the view keep working.
    exec(&raw, "INSERT INTO \"active_widgets\" (\"id\") VALUES (2)").await;
    assert_eq!(
        count(&raw, "widgets").await,
        2,
        "writes through the view still work after the re-publish",
    );
}

#[tokio::test]
async fn a_view_redefined_with_different_columns_does_not_replay_its_old_trigger() {
    // A migration that changes a view's output columns (same name) is a destructive DropView+CreateView.
    // The old INSTEAD OF trigger's body may reference a now-removed column, so it must NOT be replayed —
    // SQLite would recreate it cleanly and then fail writes at runtime. Replay compares captured vs
    // recreated view columns and skips on a shape change.
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish widgets + active_widgets(id) view");
    exec(
        &raw,
        "CREATE TRIGGER \"active_widgets_insert\" INSTEAD OF INSERT ON \"active_widgets\" \
         BEGIN INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (NEW.\"id\", 1); END",
    )
    .await;

    // v2 redefines active_widgets to output a differently-named column (a column-set change → destructive
    // DropView + CreateView).
    let mut renamed_view = active_widgets_view();
    renamed_view.columns[0].name = "widget_id".to_owned();
    renamed_view.query.projection[0].output_name = "widget_id".to_owned();
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widget_table()],
            views: vec![renamed_view],
        }],
    };
    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the view redefinition");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the view redefinition");

    assert!(
        !trigger_exists(&raw, "active_widgets_insert").await,
        "an INSTEAD OF trigger must not be replayed onto a view redefined with a different shape",
    );
}

#[tokio::test]
async fn a_view_redefined_over_a_different_source_does_not_replay_its_old_trigger() {
    // Even when the output columns are unchanged, a view whose *body* changes (here the source table is
    // swapped) is a redefinition, and the old trigger body may reference the old source. Replay compares
    // the view's stored definition (not just its columns), so a body change skips replay.
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish widgets + active_widgets(id) view over widgets");
    exec(
        &raw,
        "CREATE TRIGGER \"active_widgets_insert\" INSTEAD OF INSERT ON \"active_widgets\" \
         BEGIN INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (NEW.\"id\", 1); END",
    )
    .await;

    // v2 re-points the same-columned view at a new `widgets2` table (a body change, same output `id`).
    let mut view_over_widgets2 = active_widgets_view();
    view_over_widgets2.query.from = Some(SourceRef {
        schema: None,
        name: "widgets2".to_owned(),
        alias: "q0_0".to_owned(),
    });
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![
                widget_table(),
                table(
                    "widgets2",
                    vec![
                        column("id", SqlType::I64, false),
                        column("active", SqlType::I64, false),
                    ],
                ),
            ],
            views: vec![view_over_widgets2],
        }],
    };
    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the view body change");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the view body change");

    assert!(
        !trigger_exists(&raw, "active_widgets_insert").await,
        "an INSTEAD OF trigger must not be replayed across a view body change",
    );
}

#[tokio::test]
async fn a_batch_is_not_aborted_by_a_triggered_view_whose_body_is_broken() {
    // A triggered view whose base table was dropped out of band is unanalyzable. Capture reads its stored
    // definition straight from `sqlite_master` (no view-body analysis), so it must not error — otherwise
    // even a `DROP VIEW` of the broken view would abort during pre-capture and strand the database.
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
    // Break the view: drop its base table out of band (leaves `active_widgets` unanalyzable).
    exec(&raw, "DROP TABLE \"widgets\"").await;

    // A `DROP VIEW` of the broken view through the executor must still succeed.
    connection
        .execute_ddl("DROP VIEW \"active_widgets\"")
        .await
        .expect("dropping a broken triggered view must not abort during pre-capture");

    assert!(
        !view_exists(&raw, "active_widgets").await,
        "the broken view was dropped",
    );
}

#[tokio::test]
async fn a_trigger_declared_with_different_target_casing_is_preserved() {
    // SQLite resolves object names case-insensitively but stores `sqlite_master.tbl_name` with the
    // trigger statement's casing. A trigger declared `ON active_widgets` for a view the model spells
    // `active_widgets` matches directly, so exercise the mismatch: declare the trigger against an
    // upper-cased target. The capture join is `COLLATE NOCASE`, so the trigger is still found and
    // replayed across the view re-apply.
    let (mut connection, raw) = setup().await;
    publish(&table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("publish table + view");
    exec(
        &raw,
        "CREATE TRIGGER \"active_widgets_insert\" INSTEAD OF INSERT ON \"ACTIVE_WIDGETS\" \
         BEGIN INSERT INTO \"widgets\" (\"id\", \"active\") VALUES (NEW.\"id\", 1); END",
    )
    .await;

    let plan = plan_from_database(&table_and_view(), &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the unchanged re-publish");
    apply_plan(&plan, &table_and_view(), &Sqlite, &mut connection)
        .await
        .expect("re-apply the unchanged model");

    assert!(
        trigger_exists(&raw, "active_widgets_insert").await,
        "a trigger whose target casing differs must still be preserved across the view re-apply",
    );
}

#[tokio::test]
async fn a_temp_qualified_drop_trigger_does_not_suppress_a_persistent_view_trigger() {
    // An explicit `DROP TRIGGER temp.foo` targets the temp schema, so it must not suppress replay of a
    // same-named *persistent* view trigger that the batch's view re-apply cascades. The scan preserves
    // the schema qualifier; only an unqualified/`main.` drop suppresses the persistent trigger.
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

    // One batch: a temp-qualified DROP TRIGGER (of a non-existent temp trigger) plus a byte-identical view
    // re-apply (re-emitting the view's own stored definition) that cascades the persistent trigger. The
    // persistent trigger must be replayed, not suppressed by the temp-qualified drop.
    let view_definition = view_sql(&raw, "active_widgets").await;
    connection
        .execute_ddl(&format!(
            "DROP TRIGGER IF EXISTS temp.\"active_widgets_insert\"; \
             DROP VIEW \"active_widgets\"; {view_definition}",
        ))
        .await
        .expect("re-apply the view alongside a temp-qualified trigger drop");

    assert!(
        trigger_exists(&raw, "active_widgets_insert").await,
        "a temp-qualified DROP TRIGGER must not suppress the persistent view trigger",
    );
}

#[tokio::test]
async fn an_explicit_drop_trigger_through_the_executor_is_honored() {
    // Trigger replay must not undo an intentional removal: a caller that runs `DROP TRIGGER` through the
    // DDL executor (leaving the target view in place) means the trigger to be gone. The batch scan marks
    // it explicitly dropped, so it is not resurrected even though its target survives.
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

    connection
        .execute_ddl("DROP TRIGGER \"active_widgets_insert\"")
        .await
        .expect("drop the trigger through the executor");

    assert!(
        !trigger_exists(&raw, "active_widgets_insert").await,
        "an explicit DROP TRIGGER must not be undone by trigger replay",
    );
    assert_eq!(
        count(&raw, "active_widgets").await,
        0,
        "the view itself is untouched by the trigger drop",
    );
}

#[tokio::test]
async fn an_explicit_single_quoted_drop_trigger_is_honored() {
    // SQLite accepts a single-quoted trigger name in `DROP TRIGGER 'x'` (treating the string as an
    // identifier). The batch scan must recognize that form too, or replay would resurrect the trigger the
    // caller just dropped.
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

    connection
        .execute_ddl("DROP TRIGGER 'active_widgets_insert'")
        .await
        .expect("drop the trigger via single-quoted name");

    assert!(
        !trigger_exists(&raw, "active_widgets_insert").await,
        "a single-quoted explicit DROP TRIGGER must not be undone by trigger replay",
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
async fn replacing_a_triggered_table_with_a_same_named_view_does_not_replay_the_trigger() {
    // Only view triggers are captured, so a table's `AFTER` trigger is never a replay candidate. A
    // table→view swap must therefore not roll back trying to recreate that trigger on the replacement
    // view (SQLite only allows `INSTEAD OF` on views); the trigger is legitimately gone with its table.
    let (mut connection, raw) = setup().await;
    let v1 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![
                widget_table(),
                table("summary", vec![column("id", SqlType::I64, false)]),
            ],
            views: Vec::new(),
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
    // A user attaches an AFTER INSERT trigger to the `summary` table — invalid on a view.
    exec(
        &raw,
        "CREATE TRIGGER \"summary_after_insert\" AFTER INSERT ON \"summary\" \
         BEGIN SELECT 1; END",
    )
    .await;

    // v2 swaps the `summary` table for a same-named view over `widgets`.
    let mut summary_view = active_widgets_view();
    summary_view.name = "summary".to_owned();
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widget_table()],
            views: vec![summary_view],
        }],
    };
    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the table→view replacement");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("the table→view swap must not roll back on trigger replay");
    assert_eq!(
        count(&raw, "summary").await,
        1,
        "the same-named view resolves after replacing the triggered table",
    );
    assert!(
        !trigger_exists(&raw, "summary_after_insert").await,
        "the table's AFTER trigger must not be resurrected onto the replacement view",
    );
}

#[tokio::test]
async fn a_table_trigger_is_not_preserved_across_a_rebuild() {
    // Trigger preservation is scoped to view (`INSTEAD OF`) triggers. A table's `AFTER`/`BEFORE` trigger
    // is intentionally NOT replayed across a rebuild: a rebuild that drops or renames a column would
    // otherwise recreate a trigger whose `NEW`/`OLD` body references the old column, which SQLite accepts
    // at CREATE time and only rejects when it fires — a silent break. A rebuild dropping the table's
    // trigger (pre-existing SQLite behavior) is preferable to resurrecting a stale one.
    let (mut connection, raw) = setup().await;
    publish(&one_table(widget_table()), &Sqlite, &mut connection)
        .await
        .expect("publish widgets");
    exec(
        &raw,
        "CREATE TRIGGER \"widgets_after_insert\" AFTER INSERT ON \"widgets\" BEGIN SELECT 1; END",
    )
    .await;

    // A UNIQUE is inline-only in SQLite, so adding one forces a create-copy-drop-rename rebuild.
    let mut widgets = widget_table();
    widgets.uniques.push(Constraint {
        name: String::new(),
        columns: vec!["id".to_owned()],
    });
    let v2 = one_table(widgets);
    let plan = plan_from_database(&v2, &mut connection, DiffPolicy::ALLOW_ALL)
        .await
        .expect("plan the rebuild");
    apply_plan(&plan, &v2, &Sqlite, &mut connection)
        .await
        .expect("apply the rebuild");

    assert!(
        !trigger_exists(&raw, "widgets_after_insert").await,
        "a table trigger is not preserved across a rebuild (scoped out to avoid stale-column replay)",
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
    view_x.query.from = Some(SourceRef {
        schema: None,
        name: "y".to_owned(),
        alias: "q0_0".to_owned(),
    });
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
    renamed.query.projection[0].output_name = "widget_id".to_owned();
    let v2 = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![widget_table()],
            views: vec![renamed],
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
    view_thing.query.from = Some(SourceRef {
        schema: None,
        name: "renamed".to_owned(),
        alias: "q0_0".to_owned(),
    });
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
