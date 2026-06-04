//! CLI integration tests.
//!
//! `rejects_injection` is fast (validation fails before any build). `extracts_and_scripts` actually
//! compiles + runs an extraction stub against the `tests/fixtures/sample` crate, so it is `#[ignore]`d
//! (slow, nested cargo); run it with `cargo test -p squealy-cli -- --ignored`.

use std::process::Command;

use squealy_model::{
    CheckModel, ColumnModel, ConstraintDeferrability, DatabaseModel, DdlExecutor, ForeignKeyMatch,
    ForeignKeyModel, IndexModel, RefactorLog, RefactorOperation, RenameColumn, RenameTable,
    SchemaConnect, SchemaModel, SchemaRefactorStore, SqlType, TableModel, read_package,
    write_package, write_package_with_refactors,
};
use squealy_mysql::Mysql;
use squealy_postgresql::Postgres;

const SQUEALY: &str = env!("CARGO_BIN_EXE_squealy");
const POSTGRES_RESET_SCHEMAS: &str = "\
DO $$
DECLARE
    schema_name text;
BEGIN
    FOR schema_name IN
        SELECT nspname
        FROM pg_namespace
        WHERE nspname NOT IN ('pg_catalog', 'information_schema')
          AND nspname NOT LIKE 'pg_toast%'
    LOOP
        EXECUTE format('DROP SCHEMA IF EXISTS %I CASCADE', schema_name);
    END LOOP;
END
$$";
const POSTGRES_RESTORE_PUBLIC_SCHEMA: &str = "CREATE SCHEMA IF NOT EXISTS \"public\"";

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
fn publish_help_exposes_incremental_policy_flags() {
    let output = Command::new(SQUEALY)
        .args(["publish", "--help"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--incremental"),
        "help should include incremental publish mode: {stdout}"
    );
    assert!(
        stdout.contains("--report"),
        "help should include incremental report mode: {stdout}"
    );
    assert!(
        stdout.contains("--allow-ambiguous"),
        "help should include ambiguous-change policy flag: {stdout}"
    );
    assert!(
        stdout.contains("--allow-destructive"),
        "help should include destructive-change policy flag: {stdout}"
    );
}

#[test]
fn status_help_explains_live_package_comparison() {
    let output = Command::new(SQUEALY)
        .args(["status", "--help"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("live database state"),
        "help should explain live database comparison: {stdout}"
    );
    assert!(
        stdout.contains("--package"),
        "help should accept package input: {stdout}"
    );
    assert!(
        stdout.contains("--url"),
        "help should include connection URL option: {stdout}"
    );
    assert!(
        stdout.contains("--history"),
        "help should include publish history limit option: {stdout}"
    );
    assert!(
        stdout.contains("--check-all"),
        "help should include combined status check option: {stdout}"
    );
    assert!(
        stdout.contains("--check-schema"),
        "help should include schema check option: {stdout}"
    );
    assert!(
        stdout.contains("--check-refactors"),
        "help should include refactor check option: {stdout}"
    );
    assert!(
        stdout.contains("--check-metadata"),
        "help should include metadata check option: {stdout}"
    );
}

#[test]
fn refactors_help_explains_package_comparison() {
    let output = Command::new(SQUEALY)
        .args(["refactors", "--help"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("list"),
        "help should include the list subcommand: {stdout}"
    );
    assert!(
        stdout.contains("repair"),
        "help should include the repair subcommand: {stdout}"
    );

    let output = Command::new(SQUEALY)
        .args(["refactors", "list", "--help"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "list help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("applied schema refactors"),
        "list help should explain recorded refactor reporting: {stdout}"
    );
    assert!(
        stdout.contains("--package"),
        "list help should include package comparison option: {stdout}"
    );
    assert!(
        stdout.contains("--url"),
        "list help should include connection URL option: {stdout}"
    );

    let output = Command::new(SQUEALY)
        .args(["refactors", "repair", "--help"])
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "repair help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("validating that the live schema already reflects them"),
        "repair help should explain final-state validation: {stdout}"
    );
    assert!(
        stdout.contains("--package"),
        "repair help should require package input: {stdout}"
    );
}

#[test]
fn publish_report_requires_incremental_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    write_package(&empty_model(), &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["publish", "--report", "--package"])
        .arg(&package)
        .args(["--url", "postgres://unused"])
        .output()
        .expect("run squealy");

    assert!(!output.status.success(), "invalid report mode should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--report currently requires --incremental"),
        "unexpected stderr: {stderr}"
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
fn diff_reports_no_changes_for_identical_packages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("schema.sqz");
    write_package(&empty_model(), &package).expect("write package");

    let output = Command::new(SQUEALY)
        .args(["diff", "--desired"])
        .arg(&package)
        .args(["--actual"])
        .arg(&package)
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "no changes\n");
}

#[test]
fn diff_reports_package_model_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let desired = dir.path().join("desired.sqz");
    let actual = dir.path().join("actual.sqz");
    let mut desired_model = empty_model();
    desired_model.schemas[0].tables[0]
        .columns
        .push(ColumnModel {
            name: "name".to_owned(),
            comment: None,
            ty: SqlType::Text,
            collation: None,
            nullable: false,
            default: None,
            identity: None,
            generated: None,
        });
    desired_model.schemas[0].tables.push(TableModel {
        name: "created".to_owned(),
        comment: None,
        columns: vec![],
        primary_key: None,
        foreign_keys: vec![],
        uniques: vec![],
        checks: vec![],
        indexes: vec![],
    });
    write_package(&desired_model, &desired).expect("write desired package");
    write_package(&empty_model(), &actual).expect("write actual package");

    let output = Command::new(SQUEALY)
        .args(["diff", "--desired"])
        .arg(&desired)
        .args(["--actual"])
        .arg(&actual)
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("safe table + public.created"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        stdout.contains("ambiguous table ~ public.events"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        stdout.contains("ambiguous column + public.events.name"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn diff_policy_check_rejects_blocked_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let desired = dir.path().join("desired.sqz");
    let actual = dir.path().join("actual.sqz");
    let mut desired_model = empty_model();
    desired_model.schemas[0].tables[0]
        .columns
        .push(required_text_column("name"));
    write_package(&desired_model, &desired).expect("write desired package");
    write_package(&empty_model(), &actual).expect("write actual package");

    let output = Command::new(SQUEALY)
        .args(["diff", "--check-policy", "--desired"])
        .arg(&desired)
        .args(["--actual"])
        .arg(&actual)
        .output()
        .expect("run squealy");

    assert!(!output.status.success(), "blocked diff should fail");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ambiguous column + public.events.name"),
        "unexpected stdout: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("blocked change"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn diff_policy_check_allows_requested_ambiguous_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let desired = dir.path().join("desired.sqz");
    let actual = dir.path().join("actual.sqz");
    let mut desired_model = empty_model();
    desired_model.schemas[0].tables[0]
        .columns
        .push(required_text_column("name"));
    write_package(&desired_model, &desired).expect("write desired package");
    write_package(&empty_model(), &actual).expect("write actual package");

    let output = Command::new(SQUEALY)
        .args(["diff", "--check-policy", "--allow-ambiguous", "--desired"])
        .arg(&desired)
        .args(["--actual"])
        .arg(&actual)
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "diff should pass: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn plan_renders_incremental_sql_between_packages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let desired = dir.path().join("desired.sqz");
    let actual = dir.path().join("actual.sqz");
    write_package(&empty_model(), &desired).expect("write desired package");
    write_package(&DatabaseModel { schemas: vec![] }, &actual).expect("write actual package");

    let output = Command::new(SQUEALY)
        .args(["plan", "--backend", "postgres", "--desired"])
        .arg(&desired)
        .args(["--actual"])
        .arg(&actual)
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("CREATE SCHEMA IF NOT EXISTS \"public\""),
        "unexpected stdout: {stdout}"
    );
    assert!(
        stdout.contains("CREATE TABLE \"public\".\"events\""),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn plan_blocks_ambiguous_changes_unless_allowed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let desired = dir.path().join("desired.sqz");
    let actual = dir.path().join("actual.sqz");
    let mut desired_model = empty_model();
    desired_model.schemas[0].tables[0]
        .columns
        .push(required_text_column("name"));
    write_package(&desired_model, &desired).expect("write desired package");
    write_package(&empty_model(), &actual).expect("write actual package");

    let blocked = Command::new(SQUEALY)
        .args(["plan", "--backend", "postgres", "--desired"])
        .arg(&desired)
        .args(["--actual"])
        .arg(&actual)
        .output()
        .expect("run squealy");
    assert!(!blocked.status.success(), "ambiguous plan should fail");
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        stderr.contains("blocked change"),
        "unexpected stderr: {stderr}"
    );

    let allowed = Command::new(SQUEALY)
        .args([
            "plan",
            "--backend",
            "postgres",
            "--allow-ambiguous",
            "--desired",
        ])
        .arg(&desired)
        .args(["--actual"])
        .arg(&actual)
        .output()
        .expect("run squealy");
    assert!(
        allowed.status.success(),
        "allowed plan failed: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(
        stdout.contains("ALTER TABLE \"public\".\"events\" ADD COLUMN \"name\" text NOT NULL"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn plan_reads_refactors_from_desired_package() {
    let dir = tempfile::tempdir().expect("tempdir");
    let desired = dir.path().join("desired.sqz");
    let actual = dir.path().join("actual.sqz");

    let mut desired_model = empty_model();
    desired_model.schemas[0].tables[0]
        .columns
        .push(required_text_column("name"));

    let mut actual_model = empty_model();
    actual_model.schemas[0].tables[0]
        .columns
        .push(required_text_column("display_name"));

    let refactors = RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-event-display-name".to_owned(),
            schema: Some("public".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    };
    write_package_with_refactors(&desired_model, &refactors, &desired)
        .expect("write desired package");
    write_package(&actual_model, &actual).expect("write actual package");

    let output = Command::new(SQUEALY)
        .args(["plan", "--backend", "postgres", "--desired"])
        .arg(&desired)
        .args(["--actual"])
        .arg(&actual)
        .output()
        .expect("run squealy");

    assert!(
        output.status.success(),
        "refactor-aware plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "ALTER TABLE \"public\".\"events\" RENAME COLUMN \"display_name\" TO \"name\";"
        ),
        "unexpected stdout: {stdout}"
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

#[tokio::test]
#[ignore]
async fn postgres_incremental_publish_applies_safe_plan() {
    let url = postgres_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let base_package = dir.path().join("base.sqz");
    let desired_package = dir.path().join("desired.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let base = live_introspection_model();
    let desired = live_introspection_model_with_nullable_column("description");
    write_package(&base, &base_package).expect("write base package");
    write_package(&desired, &desired_package).expect("write desired package");

    let mut connection = Postgres.connect(&url).await.expect("connect to Postgres");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"cli_live_introspect\" CASCADE")
        .await
        .expect("drop schema");

    let publish_base = Command::new(SQUEALY)
        .args(["publish", "--backend", "postgres", "--package"])
        .arg(&base_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish_base.status.success(),
        "base publish failed: {}",
        String::from_utf8_lossy(&publish_base.stderr)
    );

    let publish_incremental = Command::new(SQUEALY)
        .args([
            "publish",
            "--incremental",
            "--backend",
            "postgres",
            "--package",
        ])
        .arg(&desired_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy incremental publish");
    assert!(
        publish_incremental.status.success(),
        "incremental publish failed: {}",
        String::from_utf8_lossy(&publish_incremental.stderr)
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

    let actual = actual_schema(
        read_package(&introspected_package).expect("read introspected package"),
        "cli_live_introspect",
    );
    assert!(
        actual.tables[0]
            .columns
            .iter()
            .any(|column| column.name == "description"),
        "incremental publish should add the description column: {actual:?}"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"cli_live_introspect\" CASCADE")
        .await
        .expect("cleanup schema");
}

#[tokio::test]
#[ignore]
async fn postgres_incremental_publish_report_does_not_apply_plan() {
    let url = postgres_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let base_package = dir.path().join("base.sqz");
    let desired_package = dir.path().join("desired.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let base = live_introspection_model();
    let desired = live_introspection_model_with_nullable_column("description");
    write_package(&base, &base_package).expect("write base package");
    write_package(&desired, &desired_package).expect("write desired package");

    let mut connection = Postgres.connect(&url).await.expect("connect to Postgres");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"cli_live_introspect\" CASCADE")
        .await
        .expect("drop schema");

    let publish_base = Command::new(SQUEALY)
        .args(["publish", "--backend", "postgres", "--package"])
        .arg(&base_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish_base.status.success(),
        "base publish failed: {}",
        String::from_utf8_lossy(&publish_base.stderr)
    );

    let report = Command::new(SQUEALY)
        .args([
            "publish",
            "--incremental",
            "--report",
            "--backend",
            "postgres",
            "--package",
        ])
        .arg(&desired_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy incremental report");
    assert!(
        report.status.success(),
        "incremental report failed: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let stdout = String::from_utf8_lossy(&report.stdout);
    assert!(
        stdout.contains(
            "ALTER TABLE \"cli_live_introspect\".\"events\" ADD COLUMN \"description\" text"
        ),
        "unexpected stdout: {stdout}"
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

    let actual = actual_schema(
        read_package(&introspected_package).expect("read introspected package"),
        "cli_live_introspect",
    );
    assert!(
        actual.tables[0]
            .columns
            .iter()
            .all(|column| column.name != "description"),
        "report should not apply the description column: {actual:?}"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS \"cli_live_introspect\" CASCADE")
        .await
        .expect("cleanup schema");
}

#[tokio::test]
#[ignore]
async fn postgres_incremental_publish_records_refactor_ids() {
    let url = postgres_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let base_package = dir.path().join("base.sqz");
    let desired_package = dir.path().join("desired.sqz");
    let report_package = dir.path().join("report.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let base = live_introspection_model_with_nullable_column("display_name");
    let desired = live_introspection_model_with_nullable_column("name");
    let refactors = live_rename_refactors();
    let report_refactors = live_refactor_report_log();
    write_package(&base, &base_package).expect("write base package");
    write_package_with_refactors(&desired, &refactors, &desired_package)
        .expect("write desired package");
    write_package_with_refactors(&desired, &report_refactors, &report_package)
        .expect("write report package");

    let mut connection = Postgres.connect(&url).await.expect("connect to Postgres");
    connection
        .execute_ddl(POSTGRES_RESET_SCHEMAS)
        .await
        .expect("reset schemas");

    let publish_base = Command::new(SQUEALY)
        .args(["publish", "--backend", "postgres", "--package"])
        .arg(&base_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish_base.status.success(),
        "base publish failed: {}",
        String::from_utf8_lossy(&publish_base.stderr)
    );

    let publish_incremental = Command::new(SQUEALY)
        .args([
            "publish",
            "--incremental",
            "--allow-ambiguous",
            "--backend",
            "postgres",
            "--package",
        ])
        .arg(&desired_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy incremental publish");
    assert!(
        publish_incremental.status.success(),
        "incremental publish failed: {}",
        String::from_utf8_lossy(&publish_incremental.stderr)
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
    assert_renamed_live_column(read_package(&introspected_package).expect("read package"));
    assert_eq!(
        connection
            .applied_refactor_ids()
            .await
            .expect("read applied refactors"),
        vec!["rename-event-display-name".to_owned()]
    );

    let refactors = Command::new(SQUEALY)
        .args(["refactors", "list", "--backend", "postgres", "--url", &url])
        .output()
        .expect("run squealy refactors");
    assert!(
        refactors.status.success(),
        "refactors failed: {}",
        String::from_utf8_lossy(&refactors.stderr)
    );
    let stdout = String::from_utf8_lossy(&refactors.stdout);
    assert!(
        stdout.contains("applied rename-event-display-name"),
        "unexpected refactors stdout: {stdout}"
    );

    let refactors = Command::new(SQUEALY)
        .args([
            "refactors",
            "list",
            "--backend",
            "postgres",
            "--url",
            &url,
            "--package",
        ])
        .arg(&report_package)
        .output()
        .expect("run squealy refactors with package");
    assert!(
        refactors.status.success(),
        "refactors package comparison failed: {}",
        String::from_utf8_lossy(&refactors.stderr)
    );
    let stdout = String::from_utf8_lossy(&refactors.stdout);
    assert!(
        stdout.contains("applied rename-event-display-name"),
        "unexpected refactors stdout: {stdout}"
    );
    assert!(
        stdout.contains("pending rename-archived-events"),
        "unexpected refactors stdout: {stdout}"
    );

    connection
        .execute_ddl(POSTGRES_RESET_SCHEMAS)
        .await
        .expect("cleanup schemas");
    connection
        .execute_ddl(POSTGRES_RESTORE_PUBLIC_SCHEMA)
        .await
        .expect("restore public schema");
}

#[tokio::test]
#[ignore]
async fn postgres_refactor_repair_records_valid_missing_refactor_ids() {
    let url = postgres_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let schema_package = dir.path().join("schema.sqz");
    let repair_package = dir.path().join("repair.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let status_package = dir.path().join("status.sqz");
    let desired = live_introspection_model_with_nullable_column("name");
    let refactors = live_rename_refactors();
    write_package(&desired, &schema_package).expect("write schema package");
    write_package_with_refactors(&desired, &refactors, &repair_package)
        .expect("write repair package");

    let mut connection = Postgres.connect(&url).await.expect("connect to Postgres");
    connection
        .execute_ddl(POSTGRES_RESET_SCHEMAS)
        .await
        .expect("reset schemas");

    let publish_schema = Command::new(SQUEALY)
        .args(["publish", "--backend", "postgres", "--package"])
        .arg(&schema_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish_schema.status.success(),
        "schema publish failed: {}",
        String::from_utf8_lossy(&publish_schema.stderr)
    );
    assert!(
        connection
            .applied_refactor_ids()
            .await
            .expect("read applied refactors")
            .is_empty(),
        "create-from-scratch publish should not record refactor metadata"
    );

    let status = Command::new(SQUEALY)
        .args(["status", "--backend", "postgres", "--package"])
        .arg(&schema_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy status");
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("metadata package.format_version match"),
        "unexpected status stdout: {stdout}"
    );
    assert!(
        stdout.contains("metadata package.content_hash match"),
        "unexpected status stdout: {stdout}"
    );
    assert!(
        stdout.contains("publish-history latest mode=create"),
        "unexpected status stdout: {stdout}"
    );

    let status_check = Command::new(SQUEALY)
        .args([
            "status",
            "--backend",
            "postgres",
            "--check-refactors",
            "--package",
        ])
        .arg(&repair_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy status refactor check");
    assert!(
        !status_check.status.success(),
        "pending refactor check should fail"
    );
    let stderr = String::from_utf8_lossy(&status_check.stderr);
    assert!(
        stderr.contains("status check failed: refactors"),
        "unexpected status stderr: {stderr}"
    );

    let repair = Command::new(SQUEALY)
        .args([
            "refactors",
            "repair",
            "--backend",
            "postgres",
            "--url",
            &url,
            "--package",
        ])
        .arg(&repair_package)
        .output()
        .expect("run squealy refactors repair");
    assert!(
        repair.status.success(),
        "refactor repair failed: {}",
        String::from_utf8_lossy(&repair.stderr)
    );
    let stdout = String::from_utf8_lossy(&repair.stdout);
    assert!(
        stdout.contains("recorded rename-event-display-name"),
        "unexpected repair stdout: {stdout}"
    );
    assert_eq!(
        connection
            .applied_refactor_ids()
            .await
            .expect("read applied refactors"),
        vec!["rename-event-display-name".to_owned()]
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
    write_package_with_refactors(&actual, &refactors, &status_package)
        .expect("write status package");

    let status = Command::new(SQUEALY)
        .args([
            "status",
            "--backend",
            "postgres",
            "--check-schema",
            "--check-refactors",
            "--package",
        ])
        .arg(&status_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy status");
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("schema clean"),
        "unexpected status stdout: {stdout}"
    );
    assert!(
        stdout.contains("applied rename-event-display-name"),
        "unexpected status stdout: {stdout}"
    );

    let publish_incremental = Command::new(SQUEALY)
        .args([
            "publish",
            "--incremental",
            "--backend",
            "postgres",
            "--package",
        ])
        .arg(&status_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy incremental publish");
    assert!(
        publish_incremental.status.success(),
        "incremental publish failed: {}",
        String::from_utf8_lossy(&publish_incremental.stderr)
    );

    let status = Command::new(SQUEALY)
        .args([
            "status",
            "--backend",
            "postgres",
            "--history",
            "2",
            "--check-all",
            "--package",
        ])
        .arg(&status_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy status with history");
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("publish-history latest mode=incremental"),
        "unexpected status stdout: {stdout}"
    );
    assert!(
        stdout.contains("publish-history entry index=2 mode=create"),
        "unexpected status stdout: {stdout}"
    );

    connection
        .execute_ddl(POSTGRES_RESET_SCHEMAS)
        .await
        .expect("cleanup schemas");
    connection
        .execute_ddl(POSTGRES_RESTORE_PUBLIC_SCHEMA)
        .await
        .expect("restore public schema");
}

#[tokio::test]
#[ignore]
async fn mysql_incremental_publish_applies_safe_plan() {
    let url = mysql_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let base_package = dir.path().join("base.sqz");
    let desired_package = dir.path().join("desired.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let base = live_introspection_model();
    let desired = live_introspection_model_with_nullable_column("description");
    write_package(&base, &base_package).expect("write base package");
    write_package(&desired, &desired_package).expect("write desired package");

    let mut connection = Mysql.connect(&url).await.expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `cli_live_introspect`")
        .await
        .expect("drop schema");

    let publish_base = Command::new(SQUEALY)
        .args(["publish", "--backend", "mysql", "--package"])
        .arg(&base_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish_base.status.success(),
        "base publish failed: {}",
        String::from_utf8_lossy(&publish_base.stderr)
    );

    let publish_incremental = Command::new(SQUEALY)
        .args([
            "publish",
            "--incremental",
            "--backend",
            "mysql",
            "--package",
        ])
        .arg(&desired_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy incremental publish");
    assert!(
        publish_incremental.status.success(),
        "incremental publish failed: {}",
        String::from_utf8_lossy(&publish_incremental.stderr)
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

    let actual = actual_schema(
        read_package(&introspected_package).expect("read introspected package"),
        "cli_live_introspect",
    );
    assert!(
        actual.tables[0]
            .columns
            .iter()
            .any(|column| column.name == "description"),
        "incremental publish should add the description column: {actual:?}"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `cli_live_introspect`")
        .await
        .expect("cleanup schema");
}

#[tokio::test]
#[ignore]
async fn mysql_incremental_publish_report_does_not_apply_plan() {
    let url = mysql_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let base_package = dir.path().join("base.sqz");
    let desired_package = dir.path().join("desired.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let base = live_introspection_model();
    let desired = live_introspection_model_with_nullable_column("description");
    write_package(&base, &base_package).expect("write base package");
    write_package(&desired, &desired_package).expect("write desired package");

    let mut connection = Mysql.connect(&url).await.expect("connect to MySQL");
    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `cli_live_introspect`")
        .await
        .expect("drop schema");

    let publish_base = Command::new(SQUEALY)
        .args(["publish", "--backend", "mysql", "--package"])
        .arg(&base_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish_base.status.success(),
        "base publish failed: {}",
        String::from_utf8_lossy(&publish_base.stderr)
    );

    let report = Command::new(SQUEALY)
        .args([
            "publish",
            "--incremental",
            "--report",
            "--backend",
            "mysql",
            "--package",
        ])
        .arg(&desired_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy incremental report");
    assert!(
        report.status.success(),
        "incremental report failed: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let stdout = String::from_utf8_lossy(&report.stdout);
    assert!(
        stdout.contains("ALTER TABLE `cli_live_introspect`.`events` ADD COLUMN `description` TEXT"),
        "unexpected stdout: {stdout}"
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

    let actual = actual_schema(
        read_package(&introspected_package).expect("read introspected package"),
        "cli_live_introspect",
    );
    assert!(
        actual.tables[0]
            .columns
            .iter()
            .all(|column| column.name != "description"),
        "report should not apply the description column: {actual:?}"
    );

    connection
        .execute_ddl("DROP SCHEMA IF EXISTS `cli_live_introspect`")
        .await
        .expect("cleanup schema");
}

#[tokio::test]
#[ignore]
async fn mysql_incremental_publish_records_refactor_ids() {
    let url = mysql_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let base_package = dir.path().join("base.sqz");
    let desired_package = dir.path().join("desired.sqz");
    let report_package = dir.path().join("report.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let base = live_introspection_model_with_nullable_column("display_name");
    let desired = live_introspection_model_with_nullable_column("name");
    let refactors = live_rename_refactors();
    let report_refactors = live_refactor_report_log();
    write_package(&base, &base_package).expect("write base package");
    write_package_with_refactors(&desired, &refactors, &desired_package)
        .expect("write desired package");
    write_package_with_refactors(&desired, &report_refactors, &report_package)
        .expect("write report package");

    let mut connection = Mysql.connect(&url).await.expect("connect to MySQL");
    connection
        .execute_ddl(
            "DROP SCHEMA IF EXISTS `cli_live_introspect`;\n\
DROP SCHEMA IF EXISTS `__squealy`",
        )
        .await
        .expect("drop schemas");

    let publish_base = Command::new(SQUEALY)
        .args(["publish", "--backend", "mysql", "--package"])
        .arg(&base_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish_base.status.success(),
        "base publish failed: {}",
        String::from_utf8_lossy(&publish_base.stderr)
    );

    let publish_incremental = Command::new(SQUEALY)
        .args([
            "publish",
            "--incremental",
            "--backend",
            "mysql",
            "--package",
        ])
        .arg(&desired_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy incremental publish");
    assert!(
        publish_incremental.status.success(),
        "incremental publish failed: {}",
        String::from_utf8_lossy(&publish_incremental.stderr)
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
    assert_renamed_live_column(read_package(&introspected_package).expect("read package"));
    assert_eq!(
        connection
            .applied_refactor_ids()
            .await
            .expect("read applied refactors"),
        vec!["rename-event-display-name".to_owned()]
    );

    let refactors = Command::new(SQUEALY)
        .args(["refactors", "list", "--backend", "mysql", "--url", &url])
        .output()
        .expect("run squealy refactors");
    assert!(
        refactors.status.success(),
        "refactors failed: {}",
        String::from_utf8_lossy(&refactors.stderr)
    );
    let stdout = String::from_utf8_lossy(&refactors.stdout);
    assert!(
        stdout.contains("applied rename-event-display-name"),
        "unexpected refactors stdout: {stdout}"
    );

    let refactors = Command::new(SQUEALY)
        .args([
            "refactors",
            "list",
            "--backend",
            "mysql",
            "--url",
            &url,
            "--package",
        ])
        .arg(&report_package)
        .output()
        .expect("run squealy refactors with package");
    assert!(
        refactors.status.success(),
        "refactors package comparison failed: {}",
        String::from_utf8_lossy(&refactors.stderr)
    );
    let stdout = String::from_utf8_lossy(&refactors.stdout);
    assert!(
        stdout.contains("applied rename-event-display-name"),
        "unexpected refactors stdout: {stdout}"
    );
    assert!(
        stdout.contains("pending rename-archived-events"),
        "unexpected refactors stdout: {stdout}"
    );

    connection
        .execute_ddl(
            "DROP SCHEMA IF EXISTS `cli_live_introspect`;\n\
DROP SCHEMA IF EXISTS `__squealy`",
        )
        .await
        .expect("cleanup schemas");
}

#[tokio::test]
#[ignore]
async fn mysql_refactor_repair_records_valid_missing_refactor_ids() {
    let url = mysql_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let schema_package = dir.path().join("schema.sqz");
    let repair_package = dir.path().join("repair.sqz");
    let introspected_package = dir.path().join("introspected.sqz");
    let status_package = dir.path().join("status.sqz");
    let desired = live_introspection_model_with_nullable_column("name");
    let refactors = live_rename_refactors();
    write_package(&desired, &schema_package).expect("write schema package");
    write_package_with_refactors(&desired, &refactors, &repair_package)
        .expect("write repair package");

    let mut connection = Mysql.connect(&url).await.expect("connect to MySQL");
    connection
        .execute_ddl(
            "DROP SCHEMA IF EXISTS `cli_live_introspect`;\n\
DROP SCHEMA IF EXISTS `__squealy`",
        )
        .await
        .expect("drop schemas");

    let publish_schema = Command::new(SQUEALY)
        .args(["publish", "--backend", "mysql", "--package"])
        .arg(&schema_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy publish");
    assert!(
        publish_schema.status.success(),
        "schema publish failed: {}",
        String::from_utf8_lossy(&publish_schema.stderr)
    );
    assert!(
        connection
            .applied_refactor_ids()
            .await
            .expect("read applied refactors")
            .is_empty(),
        "create-from-scratch publish should not record refactor metadata"
    );

    let status = Command::new(SQUEALY)
        .args(["status", "--backend", "mysql", "--package"])
        .arg(&schema_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy status");
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("metadata package.format_version match"),
        "unexpected status stdout: {stdout}"
    );
    assert!(
        stdout.contains("metadata package.content_hash match"),
        "unexpected status stdout: {stdout}"
    );
    assert!(
        stdout.contains("publish-history latest mode=create"),
        "unexpected status stdout: {stdout}"
    );

    let status_check = Command::new(SQUEALY)
        .args([
            "status",
            "--backend",
            "mysql",
            "--check-refactors",
            "--package",
        ])
        .arg(&repair_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy status refactor check");
    assert!(
        !status_check.status.success(),
        "pending refactor check should fail"
    );
    let stderr = String::from_utf8_lossy(&status_check.stderr);
    assert!(
        stderr.contains("status check failed: refactors"),
        "unexpected status stderr: {stderr}"
    );

    let repair = Command::new(SQUEALY)
        .args([
            "refactors",
            "repair",
            "--backend",
            "mysql",
            "--url",
            &url,
            "--package",
        ])
        .arg(&repair_package)
        .output()
        .expect("run squealy refactors repair");
    assert!(
        repair.status.success(),
        "refactor repair failed: {}",
        String::from_utf8_lossy(&repair.stderr)
    );
    let stdout = String::from_utf8_lossy(&repair.stdout);
    assert!(
        stdout.contains("recorded rename-event-display-name"),
        "unexpected repair stdout: {stdout}"
    );
    assert_eq!(
        connection
            .applied_refactor_ids()
            .await
            .expect("read applied refactors"),
        vec!["rename-event-display-name".to_owned()]
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
    write_package_with_refactors(&actual, &refactors, &status_package)
        .expect("write status package");

    let status = Command::new(SQUEALY)
        .args([
            "status",
            "--backend",
            "mysql",
            "--check-schema",
            "--check-refactors",
            "--package",
        ])
        .arg(&status_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy status");
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("schema clean"),
        "unexpected status stdout: {stdout}"
    );
    assert!(
        stdout.contains("applied rename-event-display-name"),
        "unexpected status stdout: {stdout}"
    );

    let publish_incremental = Command::new(SQUEALY)
        .args([
            "publish",
            "--incremental",
            "--backend",
            "mysql",
            "--package",
        ])
        .arg(&status_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy incremental publish");
    assert!(
        publish_incremental.status.success(),
        "incremental publish failed: {}",
        String::from_utf8_lossy(&publish_incremental.stderr)
    );

    let status = Command::new(SQUEALY)
        .args([
            "status",
            "--backend",
            "mysql",
            "--history",
            "2",
            "--check-all",
            "--package",
        ])
        .arg(&status_package)
        .args(["--url", &url])
        .output()
        .expect("run squealy status with history");
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("publish-history latest mode=incremental"),
        "unexpected status stdout: {stdout}"
    );
    assert!(
        stdout.contains("publish-history entry index=2 mode=create"),
        "unexpected status stdout: {stdout}"
    );

    connection
        .execute_ddl(
            "DROP SCHEMA IF EXISTS `cli_live_introspect`;\n\
DROP SCHEMA IF EXISTS `__squealy`",
        )
        .await
        .expect("cleanup schemas");
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

fn live_introspection_model_with_nullable_column(name: &str) -> DatabaseModel {
    let mut model = live_introspection_model();
    model.schemas[0].tables[0].columns.push(ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty: SqlType::Text,
        collation: None,
        nullable: true,
        default: None,
        identity: None,
        generated: None,
    });
    model
}

fn live_rename_refactors() -> RefactorLog {
    RefactorLog {
        operations: vec![RefactorOperation::RenameColumn(RenameColumn {
            id: "rename-event-display-name".to_owned(),
            schema: Some("cli_live_introspect".to_owned()),
            table: "events".to_owned(),
            from: "display_name".to_owned(),
            to: "name".to_owned(),
        })],
    }
}

fn live_refactor_report_log() -> RefactorLog {
    let mut refactors = live_rename_refactors();
    refactors
        .operations
        .push(RefactorOperation::RenameTable(RenameTable {
            id: "rename-archived-events".to_owned(),
            schema: Some("cli_live_introspect".to_owned()),
            from: "old_archived_events".to_owned(),
            to: "archived_events".to_owned(),
        }));
    refactors
}

fn assert_renamed_live_column(model: DatabaseModel) {
    let actual = actual_schema(model, "cli_live_introspect");
    assert!(
        actual.tables[0]
            .columns
            .iter()
            .any(|column| column.name == "name"),
        "incremental publish should rename display_name to name: {actual:?}"
    );
    assert!(
        actual.tables[0]
            .columns
            .iter()
            .all(|column| column.name != "display_name"),
        "incremental publish should remove display_name after rename: {actual:?}"
    );
}

fn required_text_column(name: &str) -> ColumnModel {
    ColumnModel {
        name: name.to_owned(),
        comment: None,
        ty: SqlType::Text,
        collation: None,
        nullable: false,
        default: None,
        identity: None,
        generated: None,
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
