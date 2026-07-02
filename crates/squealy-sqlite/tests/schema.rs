//! Live schema-management round-trips against an in-memory SQLite database.
//!
//! Unlike the MySQL/PostgreSQL schema tests (which need a server and are `#[ignore]`d), SQLite runs
//! in-process, so these execute in the normal `cargo test` run. They cover introspection (render →
//! introspect), the churn-free replan guarantee (introspect → diff → empty), and the backend-owned
//! bookkeeping stores.

use squealy::{
    ColumnExpr, ColumnMode, ColumnModel, ColumnName, Constraint, Database, DatabaseModel,
    ForeignKeyModel, IdentityMode, IdentityModel, IndexModel, Schema, SchemaConnect,
    SchemaMetadataStore, SchemaModel, SchemaPublishHistoryStore, SchemaRefactorStore, SqlType,
    Table, TableModel,
};
use squealy_sqlite::{Sqlite, SqliteConnection};

async fn connect() -> SqliteConnection {
    Sqlite.connect(":memory:").await.expect("open in-memory db")
}

// A derive model exercising every fact SQLite loses on the way back: an `AUTOINCREMENT` identity
// primary key, a named foreign key, a named multi-column unique, a named secondary index, and a
// `#[schema(App)]` namespace — all of which the canonicalizers must flatten for the replan to be empty.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    active: C::Type<'scope, bool>,
    bio: C::Type<'scope, Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(references(User::id, on_delete = "cascade"))]
    user_id: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
#[unique(columns = [organization_id, slug])]
struct Repository<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    organization_id: C::Type<'scope, i32>,
    #[column(index)]
    slug: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct App {
    users: User<'static, ColumnName>,
    posts: Post<'static, ColumnName>,
    repositorys: Repository<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct AppDb {
    app: App,
}

#[tokio::test]
async fn replan_after_publish_is_empty() {
    let model = DatabaseModel::from_database::<AppDb>();
    let mut connection = connect().await;

    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish create-from-scratch");

    // Re-planning the crate model against the freshly published schema must converge to an empty plan.
    // The crate model carries schema-qualified, named constraints and `ByDefault` identity, while SQLite
    // introspects an unqualified schema, unnamed constraints and `AutoIncrement`; without the
    // canonicalizers these churn as never-settling steps forever.
    let plan = squealy_model::plan_from_database(
        &model,
        &mut connection,
        squealy_model::DiffPolicy::ALLOW_ALL,
    )
    .await
    .expect("re-plan against published schema");

    assert!(
        plan.steps.is_empty(),
        "expected empty plan after publish, got: {:?}",
        plan.steps
    );
}

#[tokio::test]
async fn introspects_the_published_schema_shape() {
    let model = DatabaseModel::from_database::<AppDb>();
    let mut connection = connect().await;
    squealy_model::publish(&model, &Sqlite, &mut connection)
        .await
        .expect("publish create-from-scratch");

    let actual = squealy_model::introspect(&mut connection)
        .await
        .expect("introspect published schema");

    assert_eq!(actual, expected_introspected_model());
}

/// The concrete shape introspection reads back for [`AppDb`] — a single unqualified schema, affinity
/// types (`i32` → `I64`), unnamed constraints, and `AutoIncrement` identity.
fn expected_introspected_model() -> DatabaseModel {
    let autoincrement_id = || ColumnModel {
        name: "id".to_owned(),
        comment: None,
        ty: SqlType::I64,
        collation: None,
        nullable: false,
        default: None,
        identity: Some(IdentityModel {
            mode: IdentityMode::AutoIncrement,
        }),
        generated: None,
    };
    let plain = |name: &str, ty: SqlType, nullable: bool| ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty,
        collation: None,
        nullable,
        default: None,
        identity: None,
        generated: None,
    };
    let pk = || {
        Some(Constraint {
            name: String::new(),
            columns: vec!["id".to_owned()],
        })
    };

    DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            views: Vec::new(),
            // Tables come back in `sqlite_master` name order: posts, repositorys, users.
            tables: vec![
                TableModel {
                    name: "posts".to_owned(),
                    comment: None,
                    columns: vec![autoincrement_id(), plain("user_id", SqlType::I64, false)],
                    primary_key: pk(),
                    foreign_keys: vec![ForeignKeyModel {
                        name: String::new(),
                        columns: vec!["user_id".to_owned()],
                        references_schema: None,
                        references_table: "users".to_owned(),
                        references_columns: vec!["id".to_owned()],
                        match_type: None,
                        deferrability: None,
                        validation: None,
                        enforcement: None,
                        on_delete: Some(squealy::ForeignKeyAction::Cascade),
                        on_update: None,
                    }],
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: Vec::new(),
                },
                TableModel {
                    name: "repositorys".to_owned(),
                    comment: None,
                    columns: vec![
                        autoincrement_id(),
                        plain("organization_id", SqlType::I64, false),
                        plain("slug", SqlType::Text, false),
                    ],
                    primary_key: pk(),
                    foreign_keys: Vec::new(),
                    uniques: vec![Constraint {
                        name: String::new(),
                        columns: vec!["organization_id".to_owned(), "slug".to_owned()],
                    }],
                    checks: Vec::new(),
                    indexes: vec![IndexModel {
                        name: "idx_repositorys_slug".to_owned(),
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
                    }],
                },
                TableModel {
                    name: "users".to_owned(),
                    comment: None,
                    columns: vec![
                        autoincrement_id(),
                        plain("name", SqlType::Text, false),
                        plain("active", SqlType::I64, false),
                        plain("bio", SqlType::Text, true),
                    ],
                    primary_key: pk(),
                    foreign_keys: Vec::new(),
                    uniques: Vec::new(),
                    checks: Vec::new(),
                    indexes: Vec::new(),
                },
            ],
        }],
    }
}

#[tokio::test]
async fn refactor_store_records_and_reads_ids() {
    let mut connection = connect().await;
    // A read before any write returns empty (the bookkeeping table does not exist yet).
    assert!(connection.applied_refactor_ids().await.unwrap().is_empty());

    connection
        .record_applied_refactor_ids(&["r2".to_owned(), "r1".to_owned()])
        .await
        .expect("record ids");
    // Re-recording an existing id is ignored (INSERT OR IGNORE), and a new id is added.
    connection
        .record_applied_refactor_ids(&["r1".to_owned(), "r3".to_owned()])
        .await
        .expect("record more ids");

    assert_eq!(
        connection.applied_refactor_ids().await.unwrap(),
        vec!["r1".to_owned(), "r2".to_owned(), "r3".to_owned()]
    );
}

#[tokio::test]
async fn metadata_store_upserts_by_name() {
    let mut connection = connect().await;
    assert!(connection.schema_metadata().await.unwrap().is_empty());

    connection
        .record_schema_metadata(&[
            ("package_hash".to_owned(), "abc".to_owned()),
            ("format".to_owned(), "1".to_owned()),
        ])
        .await
        .expect("record metadata");
    // Recording the same name replaces its value (ON CONFLICT DO UPDATE).
    connection
        .record_schema_metadata(&[("package_hash".to_owned(), "def".to_owned())])
        .await
        .expect("update metadata");

    assert_eq!(
        connection.schema_metadata().await.unwrap(),
        vec![
            ("format".to_owned(), "1".to_owned()),
            ("package_hash".to_owned(), "def".to_owned()),
        ]
    );
}

#[tokio::test]
async fn publish_history_store_appends_newest_first() {
    let mut connection = connect().await;
    assert!(
        connection
            .schema_publish_history(10)
            .await
            .unwrap()
            .is_empty()
    );

    connection
        .record_schema_publish("create", "hash1", "1")
        .await
        .expect("record first publish");
    connection
        .record_schema_publish("incremental", "hash2", "1")
        .await
        .expect("record second publish");

    let history = connection.schema_publish_history(10).await.unwrap();
    assert_eq!(history.len(), 2);
    // Newest first.
    assert_eq!(history[0].mode, "incremental");
    assert_eq!(history[0].package_hash, "hash2");
    assert_eq!(history[1].mode, "create");
    // `limit` caps the result.
    assert_eq!(connection.schema_publish_history(1).await.unwrap().len(), 1);
    // `limit == 0` returns nothing.
    assert!(
        connection
            .schema_publish_history(0)
            .await
            .unwrap()
            .is_empty()
    );
}
