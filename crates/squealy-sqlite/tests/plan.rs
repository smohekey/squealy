//! Incremental schema-plan rendering and application for SQLite.
//!
//! Two halves: render-level assertions on the DDL [`Sqlite::render_plan`] emits for a diff (native
//! `ALTER TABLE` for the changes SQLite supports in place, a create-copy-drop-rename **rebuild** for
//! the rest), and live in-memory applications that prove a rebuild preserves data — including a
//! foreign-key child's rows, which a naive drop-and-recreate would cascade-delete.

use squealy::{
    ColumnModel, Constraint, DatabaseModel, DatabasePlan, ForeignKeyAction, ForeignKeyModel,
    IdentityMode, IdentityModel, IndexModel, SchemaBackend, SchemaModel, SqlType, TableModel,
};
use squealy_model::{
    CastColumn, DiffPolicy, PlanApplyOptions, RefactorLog, RefactorOperation, RenameColumn,
    apply_plan, apply_plan_with_options, introspect, plan_from_database,
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
