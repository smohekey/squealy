//! SQLite create-from-scratch DDL rendering tests.

use squealy::{
    ColumnExpr, ColumnMode, ColumnName, Database, DatabaseModel, Schema, SchemaBackend, Table,
};
use squealy_sqlite::Sqlite;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(App)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
    active: C::Type<'scope, bool>,
    score: C::Type<'scope, f64>,
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
#[primary_key(columns = [tenant_id, id])]
struct Seat<'scope, C: ColumnMode = ColumnExpr> {
    tenant_id: C::Type<'scope, i32>,
    id: C::Type<'scope, i32>,
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
    seats: Seat<'static, ColumnName>,
    repositorys: Repository<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct AppDb {
    app: App,
}

fn render() -> String {
    let model = DatabaseModel::from_database::<AppDb>();
    let mut sql = Vec::new();
    Sqlite.render_create(&model, &mut sql).unwrap();
    String::from_utf8(sql).unwrap()
}

#[test]
fn renders_autoincrement_primary_key_and_affinities() {
    let sql = render();
    // Identity column carries the primary key inline as the SQLite rowid alias; other columns map to
    // affinities, with NOT NULL on non-nullable columns and none on the nullable `Option` column.
    assert!(
        sql.contains("CREATE TABLE \"users\" (\n  \"id\" INTEGER PRIMARY KEY AUTOINCREMENT,"),
        "{sql}"
    );
    assert!(sql.contains("\"name\" TEXT NOT NULL"), "{sql}");
    assert!(sql.contains("\"active\" INTEGER NOT NULL"), "{sql}");
    assert!(sql.contains("\"score\" REAL NOT NULL"), "{sql}");
    // The nullable column has no NOT NULL.
    assert!(
        sql.contains("\"bio\" TEXT,") || sql.contains("\"bio\" TEXT\n"),
        "{sql}"
    );
    assert!(!sql.contains("\"bio\" TEXT NOT NULL"), "{sql}");
    // No table-level PRIMARY KEY constraint when the auto-increment column carries it.
    assert!(!sql.contains("PRIMARY KEY (\"id\")"), "{sql}");
}

#[test]
fn renders_inline_foreign_key() {
    let sql = render();
    assert!(
        sql.contains("FOREIGN KEY (\"user_id\") REFERENCES \"users\" (\"id\") ON DELETE CASCADE"),
        "{sql}"
    );
    // Foreign keys are inline in CREATE TABLE, never a separate ALTER (SQLite cannot add them).
    assert!(!sql.contains("ALTER TABLE"), "{sql}");
}

#[test]
fn renders_table_level_compound_primary_key() {
    let sql = render();
    // A compound primary key with no auto-increment column is a table-level constraint.
    assert!(sql.contains("PRIMARY KEY (\"tenant_id\", \"id\")"), "{sql}");
}

#[test]
fn renders_unique_constraint_and_index() {
    let sql = render();
    assert!(
        sql.contains("UNIQUE (\"organization_id\", \"slug\")"),
        "{sql}"
    );
    assert!(
        sql.contains("INDEX") && sql.contains("ON \"repositorys\" (\"slug\")"),
        "{sql}"
    );
}

#[test]
fn no_schema_qualification() {
    let sql = render();
    // SQLite has no schemas: table names are unqualified, with no CREATE SCHEMA.
    assert!(!sql.contains("CREATE SCHEMA"), "{sql}");
    assert!(!sql.contains("\"app\"."), "{sql}");
}

#[test]
fn render_plan_is_unsupported() {
    let plan = squealy::DatabasePlan::default();
    let mut sql = Vec::new();
    let error = Sqlite.render_plan(&plan, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn advertises_partial_index_capability() {
    // The renderer emits partial-index `WHERE`, so the backend must advertise it or the schema engine
    // rejects an `IndexModel::predicate` before this backend renders it. Other index metadata is off.
    let capabilities = Sqlite.capabilities();
    assert!(capabilities.indexes.predicates);
    assert!(!capabilities.indexes.expressions);
    assert!(!capabilities.indexes.include_columns);
    assert!(!capabilities.indexes.operator_classes);
}

#[test]
fn rejects_non_integer_autoincrement_column() {
    // A (hand-written / packaged) model with a non-integer identity primary key must be rejected
    // rather than silently rewritten to `INTEGER PRIMARY KEY AUTOINCREMENT`.
    use squealy::{
        ColumnModel, Constraint, DatabaseModel, IdentityMode, IdentityModel, SchemaModel, SqlType,
        TableModel,
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![TableModel {
                name: "t".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "id".to_owned(),
                    comment: None,
                    ty: SqlType::Text,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: Some(IdentityModel {
                        mode: IdentityMode::AutoIncrement,
                    }),
                    generated: None,
                }],
                primary_key: Some(Constraint {
                    name: "pk".to_owned(),
                    columns: vec!["id".to_owned()],
                }),
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
            views: Vec::new(),
        }],
    };
    let mut sql = Vec::new();
    let error = Sqlite.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn fixed_bytes_column_enforces_width_with_a_check() {
    // `BLOB` has no fixed width, so a `FixedBytes(N)` column carries a `CHECK (length("col") = N)` to
    // preserve the fixed-width invariant the core type and other backends enforce.
    use squealy::{ColumnModel, DatabaseModel, SchemaModel, SqlType, TableModel};
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![TableModel {
                name: "t".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "hash".to_owned(),
                    comment: None,
                    ty: SqlType::FixedBytes(16),
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
            views: Vec::new(),
        }],
    };
    let mut sql = Vec::new();
    Sqlite.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();
    assert!(
        sql.contains("\"hash\" BLOB NOT NULL CHECK (length(CAST(\"hash\" AS BLOB)) = 16)"),
        "{sql}"
    );
}

#[test]
fn rejects_table_name_collision_across_schemas() {
    // SQLite flattens schemas into one namespace, so the same table name in two schemas would collide.
    use squealy::{DatabaseModel, SchemaModel, TableModel};
    let users = || TableModel {
        name: "users".to_owned(),
        comment: None,
        columns: Vec::new(),
        primary_key: None,
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
    };
    let schema = |name: &str| SchemaModel {
        name: Some(name.to_owned()),
        tables: vec![users()],
        views: Vec::new(),
    };
    let model = DatabaseModel {
        schemas: vec![schema("public"), schema("archive")],
    };
    let mut sql = Vec::new();
    let error = Sqlite.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn rejects_case_insensitive_table_name_collision() {
    // SQLite compares object identifiers case-insensitively (ASCII), so `Users` and `users` are the
    // same object and must collide even though the strings differ.
    use squealy::{DatabaseModel, SchemaModel, TableModel};
    let table = |name: &str| TableModel {
        name: name.to_owned(),
        comment: None,
        columns: Vec::new(),
        primary_key: None,
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![table("Users"), table("users")],
            views: Vec::new(),
        }],
    };
    let mut sql = Vec::new();
    let error = Sqlite.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn rejects_index_name_collision_across_tables() {
    // SQLite keeps tables and indexes in one object namespace, so the same index name on two tables
    // (which is fine on the schema-aware backends) collides once schemas are flattened.
    use squealy::{ColumnModel, DatabaseModel, IndexModel, SchemaModel, SqlType, TableModel};
    let column = || ColumnModel {
        name: "x".to_owned(),
        comment: None,
        ty: SqlType::I32,
        collation: None,
        nullable: false,
        default: None,
        identity: None,
        generated: None,
    };
    let index = || IndexModel {
        name: "idx_x".to_owned(),
        columns: vec!["x".to_owned()],
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
    let table = |name: &str| TableModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![column()],
        primary_key: None,
        foreign_keys: Vec::new(),
        uniques: Vec::new(),
        checks: Vec::new(),
        indexes: vec![index()],
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![table("a"), table("b")],
            views: Vec::new(),
        }],
    };
    let mut sql = Vec::new();
    let error = Sqlite.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn render_create_rejects_views_for_now() {
    // View rendering is deferred (the shared view-body renderer emits schema-qualified sources and
    // non-SQLite scalar-function spellings); a model carrying a view errors rather than emit broken
    // DDL. Build a minimal model with a single (empty) view to exercise the guard.
    use squealy::{DatabaseModel, SchemaModel, ViewModel, ViewQueryModel};
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: vec![ViewModel {
                name: "v".to_owned(),
                comment: None,
                columns: Vec::new(),
                query: ViewQueryModel {
                    dependencies: Vec::new(),
                    distinct: false,
                    projection: Vec::new(),
                    from: None,
                    joins: Vec::new(),
                    filter: None,
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                },
            }],
        }],
    };
    let mut sql = Vec::new();
    let error = Sqlite.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}
