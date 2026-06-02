//! CLI integration tests.
//!
//! `rejects_injection` is fast (validation fails before any build). `extracts_and_scripts` actually
//! compiles + runs an extraction stub against the `tests/fixtures/sample` crate, so it is `#[ignore]`d
//! (slow, nested cargo); run it with `cargo test -p squealy-cli -- --ignored`.

use std::process::Command;

use squealy_model::{CheckModel, DatabaseModel, SchemaModel, TableModel, write_package};

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
