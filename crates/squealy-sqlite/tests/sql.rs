//! SQLite create-from-scratch DDL rendering tests.

use squealy::{
    ColumnExpr, ColumnMode, ColumnName, Database, DatabaseModel, Schema, SchemaBackend, Table,
};
use squealy_sqlite::Sqlite;

fn check_expr(sql: &str) -> squealy::ExprNode {
    squealy_parse::Reader::new(squealy_parse::SqlDialect::Sqlite)
        .read_check_expression(sql)
        .unwrap()
}

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
fn write_table_renders_single_table_via_the_model() {
    // `Backend::write_table` (the `to::<T>()` create path) lowers the table to a model and reuses the
    // model renderer, so it matches `render_create` for that table.
    use squealy::Backend;
    let tables = <App as Schema>::tables().collect::<Vec<_>>();
    let users = tables.iter().find(|t| t.name() == "users").unwrap();
    let mut sql = Vec::new();
    Sqlite.write_table(*users, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();
    assert!(
        sql.contains("CREATE TABLE \"users\" (\n  \"id\" INTEGER PRIMARY KEY AUTOINCREMENT,"),
        "{sql}"
    );
    assert!(sql.contains("\"name\" TEXT NOT NULL"), "{sql}");
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
fn render_plan_of_empty_plan_is_empty() {
    let plan = squealy::DatabasePlan::default();
    let mut sql = Vec::new();
    Sqlite
        .render_plan(&plan, &squealy::DatabaseModel::default(), &mut sql)
        .expect("an empty plan renders");
    assert!(sql.is_empty(), "{}", String::from_utf8_lossy(&sql));
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
        prefix_lengths: Vec::new(),
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
fn renders_table_check_constraint() {
    // A table-level CHECK renders inline and unnamed (`CHECK (expr)`): SQLite exposes it only in the
    // CREATE TABLE text, so introspection recovers it by parsing that text and matches it by a name
    // derived from the expression — the rendered name is redundant. (The inline `[u8; N]` width check is
    // separate — see the FixedBytes test above.)
    use squealy::{CheckModel, ColumnModel, DatabaseModel, SchemaModel, SqlType, TableModel};
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![TableModel {
                name: "accounts".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "balance".to_owned(),
                    comment: None,
                    ty: SqlType::I64,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: vec![CheckModel {
                    // The declared name is not rendered (SQLite checks are unnamed inline).
                    name: "ck_balance".to_owned(),
                    expression: check_expr("balance >= 0"),
                    validation: None,
                    enforcement: None,
                }],
                indexes: Vec::new(),
            }],
            views: Vec::new(),
        }],
    };
    let mut sql = Vec::new();
    Sqlite.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();
    assert!(sql.contains("CHECK ((\"balance\" >= 0))"), "{sql}");
    // Rendered unnamed — the constraint name never reaches the DDL.
    assert!(!sql.contains("ck_balance"), "{sql}");
}

#[test]
fn rejects_check_constraint_validation_or_enforcement_metadata() {
    // SQLite has no `NOT VALID`/`NOT ENFORCED` for a check, so a (hand-written / packaged) model whose
    // check carries that metadata is rejected rather than rendered as a plain, immediately-enforced
    // constraint that silently drops it — matching how `write_foreign_key` rejects the same metadata.
    use squealy::{
        CheckModel, ColumnModel, ConstraintEnforcement, ConstraintValidation, DatabaseModel,
        SchemaModel, SqlType, TableModel,
    };
    let model = |check: CheckModel| DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![TableModel {
                name: "accounts".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "balance".to_owned(),
                    comment: None,
                    ty: SqlType::I64,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: vec![check],
                indexes: Vec::new(),
            }],
            views: Vec::new(),
        }],
    };
    let base = || CheckModel {
        name: "ck_accounts_balance".to_owned(),
        expression: check_expr("balance >= 0"),
        validation: None,
        enforcement: None,
    };

    for check in [
        CheckModel {
            validation: Some(ConstraintValidation::NotValidated),
            ..base()
        },
        CheckModel {
            enforcement: Some(ConstraintEnforcement::NotEnforced),
            ..base()
        },
    ] {
        let mut sql = Vec::new();
        let error = Sqlite.render_create(&model(check), &mut sql).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
}

#[test]
fn renders_column_collation() {
    // A column's COLLATE clause renders after its type: SQLite exposes it only in the CREATE TABLE text,
    // so introspection recovers it by parsing that text.
    use squealy::{ColumnModel, DatabaseModel, SchemaModel, SqlType, TableModel};
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![TableModel {
                name: "t".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "name".to_owned(),
                    comment: None,
                    ty: SqlType::Text,
                    collation: Some("NOCASE".to_owned()),
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
    // The collation name is quoted like any identifier (so a name needing quoting still parses).
    assert!(
        sql.contains("\"name\" TEXT COLLATE \"NOCASE\" NOT NULL"),
        "{sql}"
    );
}

#[test]
fn rejects_reserved_object_name_prefix() {
    // A user table using the `__squealy_` prefix would collide with the schema-management bookkeeping
    // tables and be filtered out by introspection (churning create/drop), so rendering must reject it.
    use squealy::{DatabaseModel, SchemaModel, TableModel};
    let model = |table_name: &str| DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: vec![TableModel {
                name: table_name.to_owned(),
                comment: None,
                columns: Vec::new(),
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
            }],
            views: Vec::new(),
        }],
    };
    for reserved in ["__squealy_refactors", "sqlite_stat1"] {
        let mut sql = Vec::new();
        let error = Sqlite
            .render_create(&model(reserved), &mut sql)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported, "{reserved}");
    }
}

/// A `q0_0.<column>` reference, the alias every single-source view body uses.
fn view_col(column: &str) -> squealy::ExprNode {
    squealy::ExprNode::Column {
        alias: "q0_0".to_owned(),
        column: column.to_owned(),
    }
}

/// A single-`id`-column view selecting `id` from `app.<from>`, with an optional `WHERE`.
fn id_view(name: &str, from: &str, filter: Option<squealy::ExprNode>) -> squealy::ViewModel {
    squealy::ViewModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![squealy::ViewColumnModel {
            name: "id".to_owned(),
            ty: squealy::SqlType::I32,
            nullable: false,
        }],
        query: squealy::ViewBody::Select(Box::new(squealy::ViewQueryModel {
            dependencies: Vec::new(),
            distinct: false,
            projection: vec![squealy::ProjectionItem {
                output_name: "id".to_owned(),
                internal_alias: None,
                expr: view_col("id"),
            }],
            from: Some(squealy::SourceItem::Named(squealy::SourceRef {
                schema: Some("app".to_owned()),
                name: from.to_owned(),
                alias: "q0_0".to_owned(),
            })),
            joins: Vec::new(),
            filter,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })),
    }
}

/// A `TableModel` with a single non-null `id` column.
fn id_table(name: &str) -> squealy::TableModel {
    squealy::TableModel {
        name: name.to_owned(),
        comment: None,
        columns: vec![squealy::ColumnModel {
            name: "id".to_owned(),
            comment: None,
            ty: squealy::SqlType::I32,
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
    }
}

/// `id > 0`, a simple view `WHERE` predicate.
fn id_gt_zero() -> squealy::ExprNode {
    squealy::ExprNode::Compare {
        op: squealy::CompareOp::GreaterThan,
        left: Box::new(view_col("id")),
        right: Box::new(squealy::ExprNode::Literal("0".to_owned())),
    }
}

#[test]
fn render_create_renders_views_unqualified_in_dependency_order() {
    // SQLite has no schemas, so a view over `app.users` renders the source unqualified — `FROM "users"`,
    // not `FROM "app"."users"`, which SQLite would read as an attached database. Views are created after
    // tables and in dependency order: a view-on-view is created after the view it selects from, even
    // when it is declared first.
    use squealy::{DatabaseModel, SchemaModel};

    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![id_table("users")],
            views: vec![
                id_view("active_user_ids", "active_users", None),
                id_view("active_users", "users", Some(id_gt_zero())),
            ],
        }],
    };

    let mut sql = Vec::new();
    Sqlite.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE VIEW \"active_users\" (\"id\") AS \
SELECT q0_0.\"id\" FROM \"users\" AS q0_0 WHERE (q0_0.\"id\" > 0)"
        ),
        "missing unqualified active_users view: {sql}"
    );
    assert!(
        sql.contains(
            "CREATE VIEW \"active_user_ids\" (\"id\") AS \
SELECT q0_0.\"id\" FROM \"active_users\" AS q0_0"
        ),
        "missing active_user_ids view: {sql}"
    );

    let table_pos = sql.find("CREATE TABLE").unwrap();
    let active_users_pos = sql.find("\"active_users\" (").unwrap();
    let active_ids_pos = sql.find("\"active_user_ids\" (").unwrap();
    assert!(table_pos < active_users_pos, "tables precede views: {sql}");
    assert!(
        active_users_pos < active_ids_pos,
        "a view is created after the view it depends on: {sql}"
    );
    // No schema qualifier leaks anywhere (SQLite reads `"app"."x"` as an attached database).
    assert!(!sql.contains("\"app\""), "schema qualifier leaked: {sql}");
}

#[test]
fn render_create_renders_view_expression_ir_in_sqlite_dialect() {
    // The shared view-body renderer spells builtins in SQLite's dialect through the same `Dialect` seams
    // the query layer uses: `length` (not `CHAR_LENGTH`), `substr(s, start, len)` (not the standard
    // `SUBSTRING(s FROM start FOR len)`), and `||` concat (not `CONCAT`).
    use squealy::{
        DatabaseModel, ExprNode, ProjectionItem, ScalarFunc, SchemaModel, SourceItem, SourceRef,
        SqlType, ViewBody, ViewColumnModel, ViewModel, ViewQueryModel,
    };

    let scalar = |func: ScalarFunc, args: Vec<ExprNode>| ExprNode::ScalarFn { func, args };
    let column = |name: &str| ViewColumnModel {
        name: name.to_owned(),
        ty: SqlType::I32,
        nullable: false,
    };

    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: Vec::new(),
            views: vec![ViewModel {
                name: "labels".to_owned(),
                comment: None,
                columns: vec![column("namelen"), column("greeting"), column("prefix")],
                query: ViewBody::Select(Box::new(ViewQueryModel {
                    dependencies: Vec::new(),
                    distinct: false,
                    projection: vec![
                        ProjectionItem {
                            output_name: "namelen".to_owned(),
                            internal_alias: None,
                            expr: scalar(ScalarFunc::Length, vec![view_col("name")]),
                        },
                        ProjectionItem {
                            output_name: "greeting".to_owned(),
                            internal_alias: None,
                            expr: scalar(
                                ScalarFunc::Concat,
                                vec![view_col("name"), ExprNode::Literal("'!'".to_owned())],
                            ),
                        },
                        ProjectionItem {
                            output_name: "prefix".to_owned(),
                            internal_alias: None,
                            expr: scalar(
                                ScalarFunc::Substring,
                                vec![
                                    view_col("name"),
                                    ExprNode::Literal("1".to_owned()),
                                    ExprNode::Literal("3".to_owned()),
                                ],
                            ),
                        },
                    ],
                    from: Some(SourceItem::Named(SourceRef {
                        schema: Some("app".to_owned()),
                        name: "users".to_owned(),
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
            }],
        }],
    };

    let mut sql = Vec::new();
    Sqlite.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains("length(q0_0.\"name\")"),
        "expected SQLite length(): {sql}"
    );
    assert!(!sql.contains("CHAR_LENGTH"), "CHAR_LENGTH leaked: {sql}");
    assert!(
        sql.contains("(q0_0.\"name\" || '!')"),
        "expected `||` concat: {sql}"
    );
    assert!(!sql.contains("CONCAT("), "CONCAT leaked: {sql}");
    assert!(
        sql.contains("substr(q0_0.\"name\", 1, 3)"),
        "expected substr(): {sql}"
    );
    assert!(
        !sql.contains("SUBSTRING"),
        "SUBSTRING FROM/FOR leaked: {sql}"
    );
}

#[test]
fn rejects_intersect_all_view_body_which_sqlite_cannot_run() {
    // SQLite allows `ALL` only after `UNION`; `INTERSECT ALL`/`EXCEPT ALL` are syntax errors. A model
    // carrying such a set-op view body must be rejected at render, not emit SQL SQLite cannot run.
    use squealy::{
        DatabaseModel, ProjectionItem, SchemaModel, SourceItem, SourceRef, SqlType, ViewBody,
        ViewColumnModel, ViewModel, ViewQueryModel, ViewSetOp,
    };

    let arm = |alias: &str| {
        ViewBody::Select(Box::new(ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                internal_alias: None,
                expr: view_col("id"),
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "users".to_owned(),
                alias: alias.to_owned(),
            })),
            ..ViewQueryModel::default()
        }))
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: Vec::new(),
            views: vec![ViewModel {
                name: "v".to_owned(),
                comment: None,
                columns: vec![ViewColumnModel {
                    name: "id".to_owned(),
                    ty: SqlType::I32,
                    nullable: false,
                }],
                query: ViewBody::Set {
                    op: ViewSetOp::Intersect,
                    all: true,
                    left: Box::new(arm("q0_0")),
                    right: Box::new(arm("q0_1")),
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

#[test]
fn rejects_set_body_with_an_alias_qualified_order_by_term() {
    // A compound `ORDER BY` binds to the set's output columns, so an arm-qualified column
    // (`q0_0."id"`) cannot resolve — it must be rejected at render, not emitted as invalid SQL.
    use squealy::{
        DatabaseModel, ExprNode, OrderItem, ProjectionItem, SchemaModel, SourceItem, SourceRef,
        SqlType, ViewBody, ViewColumnModel, ViewModel, ViewQueryModel, ViewSetOp,
    };

    let arm = |alias: &str| {
        ViewBody::Select(Box::new(ViewQueryModel {
            projection: vec![ProjectionItem {
                output_name: "id".to_owned(),
                internal_alias: None,
                expr: view_col("id"),
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "users".to_owned(),
                alias: alias.to_owned(),
            })),
            ..ViewQueryModel::default()
        }))
    };
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables: Vec::new(),
            views: vec![ViewModel {
                name: "v".to_owned(),
                comment: None,
                columns: vec![ViewColumnModel {
                    name: "id".to_owned(),
                    ty: SqlType::I32,
                    nullable: false,
                }],
                query: ViewBody::Set {
                    op: ViewSetOp::Union,
                    all: false,
                    left: Box::new(arm("q0_0")),
                    right: Box::new(arm("q0_1")),
                    // An alias-qualified column is invalid as a whole-set ORDER BY term.
                    order_by: vec![OrderItem {
                        expr: ExprNode::Column {
                            alias: "q0_0".to_owned(),
                            column: "id".to_owned(),
                        },
                        direction: None,
                        nulls: None,
                    }],
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

#[test]
fn rejects_set_body_order_by_a_name_the_left_arm_does_not_emit() {
    // A compound `ORDER BY` resolves against the leftmost arm's output names (here `total`), NOT the
    // `CREATE VIEW` column list (`n`). A whole-set `ORDER BY n` therefore does not resolve and must be
    // rejected — a valid ordinal (`1`) over that one output is accepted.
    use squealy::{
        ArithmeticOp, DatabaseModel, ExprNode, OrderItem, ProjectionItem, SchemaModel, SourceItem,
        SourceRef, SqlType, ViewBody, ViewColumnModel, ViewModel, ViewQueryModel, ViewSetOp,
    };

    let arm = |alias: &str| {
        ViewBody::Select(Box::new(ViewQueryModel {
            // Projects an *expression* aliased `total` — the compound output is named `total`.
            projection: vec![ProjectionItem {
                output_name: "total".to_owned(),
                internal_alias: None,
                expr: ExprNode::Binary {
                    op: ArithmeticOp::Multiply,
                    left: Box::new(view_col("amount")),
                    right: Box::new(ExprNode::Literal("2".to_owned())),
                },
            }],
            from: Some(SourceItem::Named(SourceRef {
                schema: None,
                name: "users".to_owned(),
                alias: alias.to_owned(),
            })),
            ..ViewQueryModel::default()
        }))
    };
    let set = |order: Vec<OrderItem>| {
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: Vec::new(),
                views: vec![ViewModel {
                    name: "v".to_owned(),
                    comment: None,
                    // View output column `n` — different from the arms' alias `total`.
                    columns: vec![ViewColumnModel {
                        name: "n".to_owned(),
                        ty: SqlType::I64,
                        nullable: false,
                    }],
                    query: ViewBody::Set {
                        op: ViewSetOp::Union,
                        all: false,
                        left: Box::new(arm("q0_0")),
                        right: Box::new(arm("q0_1")),
                        order_by: order,
                        limit: None,
                        offset: None,
                    },
                }],
            }],
        }
    };
    let render = |order: Vec<OrderItem>| {
        let mut sql = Vec::new();
        Sqlite.render_create(&set(order), &mut sql).map(|()| sql)
    };
    let term = |expr: ExprNode| OrderItem {
        expr,
        direction: None,
        nulls: None,
    };

    // `ORDER BY n` — the view column name, which the compound does not expose — is rejected.
    let error = render(vec![term(ExprNode::BareColumn {
        column: "n".to_owned(),
    })])
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    // `ORDER BY total` — the leftmost arm's actual output — is accepted.
    assert!(
        render(vec![term(ExprNode::BareColumn {
            column: "total".to_owned(),
        })])
        .is_ok()
    );
    // `ORDER BY 1` — a valid ordinal — is accepted; `ORDER BY 2` (out of range) is rejected.
    assert!(render(vec![term(ExprNode::Literal("1".to_owned()))]).is_ok());
    assert_eq!(
        render(vec![term(ExprNode::Literal("2".to_owned()))])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
}

#[test]
fn render_plan_renders_view_steps() {
    // Every view the plan touches is dropped up front (`DROP VIEW IF EXISTS`, before any table work,
    // since a rebuild's rename reparses a live view over the rebuilt table). A kept view is then
    // recreated after the drops (SQLite has no `CREATE OR REPLACE VIEW`); a removed view stays dropped.
    // Names render unqualified.
    use squealy::{DatabaseModel, DatabasePlan, DatabasePlanStep};

    let kept = id_view("active_users", "users", Some(id_gt_zero()));
    let removed = id_view("legacy_users", "users", None);
    let plan = DatabasePlan {
        steps: vec![
            DatabasePlanStep::DropView {
                schema: Some("app".to_owned()),
                view: Box::new(removed),
            },
            DatabasePlanStep::CreateView {
                schema: Some("app".to_owned()),
                view: Box::new(kept),
            },
        ],
    };

    let mut sql = Vec::new();
    Sqlite
        .render_plan(&plan, &DatabaseModel::default(), &mut sql)
        .unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // Both views are dropped up front.
    assert!(
        sql.contains("DROP VIEW IF EXISTS \"active_users\""),
        "kept view must be dropped before recreate: {sql}"
    );
    assert!(
        sql.contains("DROP VIEW IF EXISTS \"legacy_users\""),
        "removed view must be dropped: {sql}"
    );
    // The kept view is recreated (SQLite spells no `OR REPLACE`); the removed one is not.
    assert!(
        sql.contains(
            "CREATE VIEW \"active_users\" (\"id\") AS \
SELECT q0_0.\"id\" FROM \"users\" AS q0_0 WHERE (q0_0.\"id\" > 0)"
        ),
        "missing recreate of the kept view: {sql}"
    );
    assert!(
        !sql.contains("OR REPLACE"),
        "SQLite has no OR REPLACE: {sql}"
    );
    assert!(
        !sql.contains("CREATE VIEW \"legacy_users\""),
        "the removed view must not be recreated: {sql}"
    );
    // The recreate comes after every up-front drop.
    let last_drop = sql.rfind("DROP VIEW IF EXISTS").unwrap();
    let create_pos = sql.find("CREATE VIEW").unwrap();
    assert!(
        last_drop < create_pos,
        "views are dropped before any recreate: {sql}"
    );
}

#[test]
fn render_create_rejects_view_name_colliding_with_table() {
    // Tables, indexes and views share one object namespace in SQLite, so a view named like a table is a
    // collision once schemas are flattened — rejected rather than rendered as a duplicate name.
    use squealy::{DatabaseModel, SchemaModel};

    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            tables: vec![id_table("users")],
            // A view sharing the `users` table's name.
            views: vec![id_view("users", "users", None)],
        }],
    };

    let mut sql = Vec::new();
    let error = Sqlite.render_create(&model, &mut sql).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}
