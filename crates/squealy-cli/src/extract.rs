//! Extracts a [`DatabaseModel`] from the user's crate by generating, compiling, and running a stub.
//!
//! The model only materializes by running code (trait-resolved column types + owned strings), so the
//! crate must be built and run. The stub is compiled in a private temp dir and run as a subprocess,
//! which isolates the user's code from this CLI process; it writes a `.sqz` we read back.

use std::path::{Path, PathBuf};
use std::process::Command;

use squealy_model::{DatabaseModel, read_package};

use crate::{CliError, stub};

/// Builds and runs an extraction stub for `database` and returns the harvested model.
pub fn extract_model(database: &str) -> Result<DatabaseModel, CliError> {
    let validated = stub::validate_database_path(database)?;
    let package = current_package()?;
    // The stub imports the crate by its *library target* name, which may differ from the package name
    // (`[lib] name = "…"`); the dependency key still uses the package name.
    let type_path = stub::crate_relative_type_path(&package.lib_name, &validated);

    let temp = tempfile::Builder::new()
        .prefix("squealy-stub-")
        .tempdir()
        .map_err(|error| CliError::Message(format!("create temp dir: {error}")))?;
    let dir = temp.path();
    std::fs::create_dir_all(dir.join("src"))
        .map_err(|error| CliError::Message(format!("create stub src: {error}")))?;

    let crate_dir = package
        .dir
        .to_str()
        .ok_or("crate path is not valid UTF-8")?
        .to_owned();
    // Local testing points this at the in-repo crate; in the wild it is a published version.
    let model_dependency =
        std::env::var("SQUEALY_STUB_MODEL").unwrap_or_else(|_| "\"0.1.0\"".to_owned());

    std::fs::write(
        dir.join("Cargo.toml"),
        stub::stub_cargo_toml(&package.name, &crate_dir, &model_dependency),
    )
    .map_err(|error| CliError::Message(format!("write stub manifest: {error}")))?;
    std::fs::write(
        dir.join("src/main.rs"),
        stub::stub_main_rs(&package.lib_name, &type_path),
    )
    .map_err(|error| CliError::Message(format!("write stub main: {error}")))?;

    let out = dir.join("model.sqz");
    let status = Command::new(cargo())
        .arg("run")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .env("SQUEALY_STUB_OUT", &out)
        .status()
        .map_err(|error| CliError::Message(format!("run extraction stub: {error}")))?;
    if !status.success() {
        return Err(CliError::Message(
            "extraction stub failed to compile or run".to_owned(),
        ));
    }

    read_package(&out)
        .map_err(|error| CliError::Message(format!("read extracted package: {error}")))
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// The resolved package the database type lives in.
struct Package {
    /// Package name — used as the stub's dependency key.
    name: String,
    /// Library target name — used as the extern-crate path in stub code (may differ from `name`).
    lib_name: String,
    /// Package directory — the path of the stub's dependency.
    dir: PathBuf,
}

/// Resolves the current crate via `cargo metadata`.
fn current_package() -> Result<Package, CliError> {
    let output = Command::new(cargo())
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| CliError::Message(format!("run cargo metadata: {error}")))?;
    if !output.status.success() {
        return Err(CliError::Message(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| CliError::Message(format!("parse cargo metadata: {error}")))?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata returned no packages")?;

    let cwd = std::env::current_dir().map_err(|error| CliError::Message(error.to_string()))?;
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
        let resolved = Package {
            name: package["name"]
                .as_str()
                .ok_or("package without name")?
                .to_owned(),
            lib_name: library_target_name(package)?,
            dir: dir.clone(),
        };

        if dir.canonicalize().map(|d| d == cwd).unwrap_or(false) {
            return Ok(resolved);
        }
        fallback = Some(resolved);
    }

    match (packages.len(), fallback) {
        (1, Some(found)) => Ok(found),
        _ => Err(CliError::Message(
            "could not determine the current package; run `squealy` from the crate directory"
                .to_owned(),
        )),
    }
}

/// Returns the package's library target name (the extern-crate name used in `use` paths), which the
/// default underscored package name does not capture when `[lib] name = "…"` is set.
fn library_target_name(package: &serde_json::Value) -> Result<String, CliError> {
    let targets = package["targets"]
        .as_array()
        .ok_or("package without targets")?;
    targets
        .iter()
        .find(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(is_library_kind))
        })
        .and_then(|target| target["name"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            CliError::Message("crate has no library target to extract a database from".to_owned())
        })
}

fn is_library_kind(kind: &serde_json::Value) -> bool {
    // "lib", "rlib", "dylib", "cdylib", "staticlib" — but not "bin"/"test"/"example"/"proc-macro".
    matches!(
        kind.as_str(),
        Some("lib" | "rlib" | "dylib" | "cdylib" | "staticlib")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prefers_the_library_target_name_over_the_package_name() {
        // `[lib] name = "renamed_lib"` on a package called `my-app`: the extern-crate path must use
        // the library target name, not the package name.
        let package = json!({
            "name": "my-app",
            "targets": [
                { "name": "build-script-build", "kind": ["custom-build"] },
                { "name": "my_app", "kind": ["bin"] },
                { "name": "renamed_lib", "kind": ["lib"] },
            ],
        });
        assert_eq!(library_target_name(&package).unwrap(), "renamed_lib");
    }

    #[test]
    fn errors_when_there_is_no_library_target() {
        let package = json!({
            "name": "bin-only",
            "targets": [{ "name": "bin-only", "kind": ["bin"] }],
        });
        assert!(library_target_name(&package).is_err());
    }
}
