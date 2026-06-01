//! Extracts a [`DatabaseModel`] from the user's crate by generating, compiling, and running a stub.
//!
//! The model only materializes by running code (trait-resolved column types + owned strings), so the
//! crate must be built and run. The stub is compiled in a private temp dir and run as a subprocess,
//! which isolates the user's code from this CLI process; it writes a `.sqz` we read back.

use std::path::{Path, PathBuf};
use std::process::Command;

use squealy_model::{DatabaseModel, read_package};

use crate::stub;

/// Builds and runs an extraction stub for `database` and returns the harvested model.
pub fn extract_model(database: &str) -> Result<DatabaseModel, String> {
    let validated = stub::validate_database_path(database)?;
    let (crate_name, crate_dir) = current_package()?;
    let crate_ident = crate_name.replace('-', "_");
    let type_path = stub::crate_relative_type_path(&crate_ident, &validated);

    let temp = tempfile::Builder::new()
        .prefix("squealy-stub-")
        .tempdir()
        .map_err(|error| format!("create temp dir: {error}"))?;
    let dir = temp.path();
    std::fs::create_dir_all(dir.join("src"))
        .map_err(|error| format!("create stub src: {error}"))?;

    let crate_dir = crate_dir
        .to_str()
        .ok_or("crate path is not valid UTF-8")?
        .to_owned();
    // Local testing points this at the in-repo crate; in the wild it is a published version.
    let model_dependency =
        std::env::var("SQUEALY_STUB_MODEL").unwrap_or_else(|_| "\"0.1.0\"".to_owned());

    std::fs::write(
        dir.join("Cargo.toml"),
        stub::stub_cargo_toml(&crate_name, &crate_dir, &model_dependency),
    )
    .map_err(|error| format!("write stub manifest: {error}"))?;
    std::fs::write(
        dir.join("src/main.rs"),
        stub::stub_main_rs(&crate_ident, &type_path),
    )
    .map_err(|error| format!("write stub main: {error}"))?;

    let out = dir.join("model.sqz");
    let status = Command::new(cargo())
        .arg("run")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .env("SQUEALY_STUB_OUT", &out)
        .status()
        .map_err(|error| format!("run extraction stub: {error}"))?;
    if !status.success() {
        return Err("extraction stub failed to compile or run".to_owned());
    }

    read_package(&out).map_err(|error| format!("read extracted package: {error}"))
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// Resolves the current crate's package name and directory via `cargo metadata`.
fn current_package() -> Result<(String, PathBuf), String> {
    let output = Command::new(cargo())
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| format!("run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("parse cargo metadata: {error}"))?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata returned no packages")?;

    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    // Prefer the package rooted at the current directory; fall back to the sole package.
    let mut fallback = None;
    for package in packages {
        let manifest = package["manifest_path"]
            .as_str()
            .ok_or("package without manifest_path")?;
        let dir = Path::new(manifest)
            .parent()
            .ok_or("manifest path has no parent")?
            .to_path_buf();
        let name = package["name"]
            .as_str()
            .ok_or("package without name")?
            .to_owned();

        if dir.canonicalize().map(|d| d == cwd).unwrap_or(false) {
            return Ok((name, dir));
        }
        fallback = Some((name, dir));
    }

    match (packages.len(), fallback) {
        (1, Some(found)) => Ok(found),
        _ => Err(
            "could not determine the current package; run `squealy` from the crate directory"
                .to_owned(),
        ),
    }
}
