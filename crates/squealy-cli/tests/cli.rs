//! CLI integration tests.
//!
//! `rejects_injection` is fast (validation fails before any build). `extracts_and_scripts` actually
//! compiles + runs an extraction stub against the `tests/fixtures/sample` crate, so it is `#[ignore]`d
//! (slow, nested cargo); run it with `cargo test -p squealy-cli -- --ignored`.

use std::process::Command;

use squealy_model::{
    CheckModel, ColumnModel, ConstraintDeferrability, DatabaseModel, DdlExecutor, ForeignKeyMatch,
    ForeignKeyModel, IndexModel, SchemaConnect, SchemaModel, SqlType, TableModel, read_package,
    write_package,
};
use squealy_mysql::Mysql;
use squealy_postgresql::Postgres;

const SQUEALY: &str = env!("CARGO_BIN_EXE_squealy");

#[test]
fn rejects_injection() {
    let output = Command::new(SQUEALY)
        .args(["script", "--database", "Evil>(); fn x"])
        .output()
        .expect("run squealy");

    assert!(!output.status.success(), "injection attempt should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("plain Rust path"),
        "expected a path-validation error, got: {stderr}"
    );
}

#[test]
fn help_explains_backend_support_semantics() {
    let output = Command::new(SQUEALY)
        .args(["check", "--help"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("render and introspect"),
        "help should explain backend support semantics: {stdout}"
    );
    assert!(
        stdout.contains("not just SQL syntax"),
        "help should distinguish round-trip support from syntax support: {stdout}"
    );
}

#[test]
fn postgres_capabilities_are_printed() {
    let output = Command::new(SQUEALY)
        .args(["capabilities", "--backend", "postgres"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "capabilities failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_capability(&stdout, "backend=postgres");
    assert_capability(&stdout, "constraints.foreign_key_match_type=true");
    assert_capability(&stdout, "constraints.foreign_key_deferrability=true");
    assert_capability(&stdout, "constraints.foreign_key_validation=true");
    assert_capability(&stdout, "constraints.foreign_key_enforcement=false");
    assert_capability(&stdout, "constraints.check_validation=true");
    assert_capability(&stdout, "constraints.check_enforcement=false");
    assert_capability(&stdout, "indexes.predicates=true");
    assert_capability(&stdout, "indexes.expressions=true");
    assert_capability(&stdout, "indexes.include_columns=true");
    assert_capability(&stdout, "indexes.null_ordering=true");
    assert_capability(&stdout, "indexes.collations=true");
    assert_capability(&stdout, "indexes.operator_classes=true");
}

#[test]
fn mysql_capabilities_are_printed() {
    let output = Command::new(SQUEALY)
        .args(["capabilities", "--backend", "mysql"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "capabilities failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_capability(&stdout, "backend=mysql");
    assert_capability(&stdout, "constraints.foreign_key_match_type=false");
    assert_capability(&stdout, "constraints.foreign_key_deferrability=false");
    assert_capability(&stdout, "constraints.foreign_key_validation=false");
    assert_capability(&stdout, "constraints.foreign_key_enforcement=false");
    assert_capability(&stdout, "constraints.check_validation=false");
    assert_capability(&stdout, "constraints.check_enforcement=false");
    assert_capability(&stdout, "indexes.predicates=false");
    assert_capability(&stdout, "indexes.expressions=false");
    assert_capability(&stdout, "indexes.include_columns=false");
    assert_capability(&stdout, "indexes.null_ordering=false");
    assert_capability(&stdout, "indexes.collations=false");
    assert_capability(&stdout, "indexes.operator_classes=false");
}

#[test]
fn introspect_help_explains_live_database_package_export() {
    let output = Command::new(SQUEALY)
        .args(["introspect", "--help"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Introspect a live database"),
        "help should explain live database introspection: {stdout}"
    );
    assert!(
        stdout.contains("--url"),
        "help should include connection URL option: {stdout}"
    );
    assert!(
        stdout.contains("<OUTPUT>"),
        "help should include output package argument: {stdout}"
    );
}

#[test]
fn unsupported_metadata_error_explains_round_trip_requirement() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    let mut model = empty_model();
    model.schemas[0].tables[0].checks.push(CheckModel {
        name: "ck_events_id".to_owned(),
        expression: "id > 0".to_owned(),
        validation: None,
        enforcement: Some(squealy_model::ConstraintEnforcement::NotEnforced),
    });
    write_package(&model, &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["check", "--package"])
        .arg(&package)
        .output()
        .expect("run squealy");

    assert!(!output.status.success(), "unsupported package should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot render and introspect"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn checks_supported_package() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    write_package(&empty_model(), &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["check", "--package"])
        .arg(&package)
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_rejects_unsupported_package_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    let mut model = empty_model();
    model.schemas[0].tables[0].checks.push(CheckModel {
        name: "ck_events_id".to_owned(),
        expression: "id > 0".to_owned(),
        validation: None,
        enforcement: Some(squealy_model::ConstraintEnforcement::NotEnforced),
    });
    write_package(&model, &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["check", "--package"])
        .arg(&package)
        .output()
        .expect("run squealy");

    assert!(!output.status.success(), "unsupported package should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("check enforcement metadata"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn mysql_backend_rejects_postgres_only_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    let mut model = empty_model();
    model.schemas[0].tables[0].checks.push(CheckModel {
        name: "ck_events_id".to_owned(),
        expression: "id > 0".to_owned(),
        validation: Some(squealy_model::ConstraintValidation::NotValidated),
        enforcement: None,
    });
    write_package(&model, &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["check", "--backend", "mysql", "--package"])
        .arg(&package)
        .output()
        .expect("run squealy");

    assert!(!output.status.success(), "unsupported package should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("check validation metadata"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn mysql_backend_rejects_unsupported_foreign_key_shape_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    let mut model = empty_model();
    model.schemas[0].tables[0]
        .foreign_keys
        .push(ForeignKeyModel {
            name: "fk_events_user_id".to_owned(),
            columns: vec!["user_id".to_owned()],
            references_schema: None,
            references_table: "users".to_owned(),
            references_columns: vec!["id".to_owned()],
            match_type: Some(ForeignKeyMatch::Full),
            deferrability: Some(ConstraintDeferrability::InitiallyDeferred),
            validation: None,
            enforcement: None,
            on_delete: None,
            on_update: None,
        });
    write_package(&model, &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["check", "--backend", "mysql", "--package"])
        .arg(&package)
        .output()
        .expect("run squealy");

    assert!(!output.status.success(), "unsupported package should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("foreign key match metadata"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn mysql_backend_rejects_unsupported_index_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    let mut model = empty_model();
    model.schemas[0].tables[0].indexes.push(IndexModel {
        name: "idx_events_open".to_owned(),
        columns: vec!["id".to_owned()],
        expressions: vec![],
        include_columns: vec![],
        unique: false,
        method: None,
        directions: vec![],
        nulls: vec![],
        collations: vec![],
        operator_classes: vec![],
        predicate: Some("id > 0".to_owned()),
    });
    write_package(&model, &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["check", "--backend", "mysql", "--package"])
        .arg(&package)
        .output()
        .expect("run squealy");

    assert!(!output.status.success(), "unsupported package should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("partial index predicates"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn mysql_backend_renders_mysql_sql() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    write_package(&empty_model(), &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["script", "--backend", "mysql", "--package"])
        .arg(&package)
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("CREATE SCHEMA IF NOT EXISTS `public`;"),
        "{stdout}"
    );
    assert!(
        stdout.contains("CREATE TABLE `public`.`events`"),
        "{stdout}"
    );
}

#[test]
#[ignore]
fn extracts_and_scripts_from_a_crate() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample");
    let model_dep = format!(
        "{{ path = \"{}/../squealy-model\" }}",
        env!("CARGO_MANIFEST_DIR")
    );

    let output = Command::new(SQUEALY)
        .current_dir(fixture)
        .env("SQUEALY_STUB_MODEL", model_dep)
        .args(["script", "--database", "SampleDb"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "extraction failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("CREATE SCHEMA IF NOT EXISTS \"public\";"),
        "{stdout}"
    );
    assert!(
        stdout.contains("CREATE TABLE \"public\".\"widgets\" ("),
        "{stdout}"
    );
    assert!(
        stdout.contains("CONSTRAINT \"uq_widgets_name\" UNIQUE (\"name\")"),
        "{stdout}"
    );
}

#[tokio::test]
#[ignore]
async fn postgres_introspects_live_database_to_package() {
    let url = postgres_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let source_package = dir.path().join("source.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let model = live_introspection_model();
    write_package(&model, &source_package).expect("write package");

    let mut connection = Postgres.connect(&url).await.expect("connect to Postgres");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"cli_live_introspect\" CASCADE")
        .await
        .expect("drop schema");

    let publish = Command::new(SQUEALY)
        .args(["publish", "--backend", "postgres", "--package"])
        .arg(&source_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish.status.success(),
        "publish failed: {}",
        String::from_utf8_lossy(&publish.stderr)
    );

    let introspect = Command::new(SQUEALY)
        .args(["introspect", "--backend", "postgres", "--url", &url])
        .arg(&introspected_package)
        .output()
        .expect("run squealy introspect");
    assert!(
        introspect.status.success(),
        "introspect failed: {}",
        String::from_utf8_lossy(&introspect.stderr)
    );

    let actual = read_package(&introspected_package).expect("read introspected package");
    assert_eq!(
        actual_schema(actual, "cli_live_introspect"),
        model.schemas[0],
        "introspected package should include the published schema"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"cli_live_introspect\" CASCADE")
        .await
        .expect("cleanup schema");
}

#[tokio::test]
#[ignore]
async fn mysql_introspects_live_database_to_package() {
    let url = mysql_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let source_package = dir.path().join("source.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let model = live_introspection_model();
    write_package(&model, &source_package).expect("write package");

    let mut connection = Mysql.connect(&url).await.expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `cli_live_introspect`")
        .await
        .expect("drop schema");

    let publish = Command::new(SQUEALY)
        .args(["publish", "--backend", "mysql", "--package"])
        .arg(&source_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish.status.success(),
        "publish failed: {}",
        String::from_utf8_lossy(&publish.stderr)
    );

    let introspect = Command::new(SQUEALY)
        .args(["introspect", "--backend", "mysql", "--url", &url])
        .arg(&introspected_package)
        .output()
        .expect("run squealy introspect");
    assert!(
        introspect.status.success(),
        "introspect failed: {}",
        String::from_utf8_lossy(&introspect.stderr)
    );

    let actual = read_package(&introspected_package).expect("read introspected package");
    assert_eq!(
        actual_schema(actual, "cli_live_introspect"),
        model.schemas[0],
        "introspected package should include the published schema"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `cli_live_introspect`")
        .await
        .expect("cleanup schema");
}

fn empty_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("public".to_owned()),
            tables: vec![TableModel {
                name: "events".to_owned(),
                comment: None,
                columns: vec![],
                primary_key: None,
                foreign_keys: vec![],
                uniques: vec![],
                checks: vec![],
                indexes: vec![],
            }],
        }],
    }
}

fn live_introspection_model() -> DatabaseModel {
    DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("cli_live_introspect".to_owned()),
            tables: vec![TableModel {
                name: "events".to_owned(),
                comment: None,
                columns: vec![ColumnModel {
                    name: "id".to_owned(),
                    comment: None,
                    ty: SqlType::I32,
                    collation: None,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: vec![],
                uniques: vec![],
                checks: vec![],
                indexes: vec![],
            }],
        }],
    }
}

fn actual_schema(model: DatabaseModel, name: &str) -> SchemaModel {
    model
        .schemas
        .into_iter()
        .find(|schema| schema.name.as_deref() == Some(name))
        .expect("schema should be present")
}

fn postgres_url() -> String {
    std::env::var("SQUEALY_POSTGRES_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:55432/squealy_test".to_owned())
}

fn mysql_url() -> String {
    std::env::var("SQUEALY_MYSQL_URL")
        .unwrap_or_else(|_| "mysql://root:root@127.0.0.1:33306/squealy_test".to_owned())
}

fn assert_capability(stdout: &str, expected: &str) {
    assert!(
        stdout.lines().any(|line| line == expected),
        "missing {expected:?} in stdout:\n{stdout}"
    );
}
