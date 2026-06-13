//! Generation of the throwaway extraction stub, plus the strict validation that keeps the
//! user-supplied `--database` path from injecting arbitrary code into it.

use crate::CliError;

/// Validates `--database` as a bare Rust path (`ident(::ident)*`).
///
/// The value is interpolated into generated source, so anything beyond identifiers and `::` — generics,
/// whitespace, punctuation — is rejected to prevent code injection when the argument is not trusted.
pub fn validate_database_path(input: &str) -> Result<String, CliError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CliError::Message(
            "`--database` must not be empty".to_owned(),
        ));
    }

    let segments = trimmed.split("::").collect::<Vec<_>>();
    for segment in &segments {
        if !is_plain_ident(segment) {
            return Err(CliError::Message(format!(
                "`--database` must be a plain Rust path (ident::ident); invalid segment `{segment}`"
            )));
        }
    }

    Ok(segments.join("::"))
}

fn is_plain_ident(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Returns the type path relative to its crate, stripping a redundant leading `<crate_ident>::`.
///
/// So `--database AppDatabase` and `--database my_crate::AppDatabase` both resolve to `AppDatabase`
/// inside `my_crate`.
pub fn crate_relative_type_path(crate_ident: &str, database_path: &str) -> String {
    database_path
        .strip_prefix(&format!("{crate_ident}::"))
        .unwrap_or(database_path)
        .to_owned()
}

/// Renders the stub `main.rs`. It walks the named database into a model and writes a `.sqz` package
/// to the path given in `SQUEALY_STUB_OUT` at runtime.
pub fn stub_main_rs(crate_ident: &str, type_path: &str) -> String {
    format!(
        "fn main() {{\n\
         \x20   let out = ::std::env::var(\"SQUEALY_STUB_OUT\")\n\
         \x20       .expect(\"SQUEALY_STUB_OUT must be set by the squealy CLI\");\n\
         \x20   let model = ::squealy_model::DatabaseModel::from_database::<::{crate_ident}::{type_path}>();\n\
         \x20   ::squealy_model::write_package(&model, ::std::path::Path::new(&out))\n\
         \x20       .expect(\"write schema package\");\n\
         }}\n"
    )
}

/// Renders the stub `Cargo.toml`. `model_dependency` is the right-hand side of the `squealy-model`
/// dependency (a version like `\"0.1.0\"` in the wild, or a `{{ path = … }}` table for local testing).
pub fn stub_cargo_toml(
    user_crate_name: &str,
    user_crate_manifest_dir: &str,
    model_dependency: &str,
) -> String {
    format!(
        "[package]\n\
         name = \"squealy-stub\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         [[bin]]\n\
         name = \"squealy-stub\"\n\
         path = \"src/main.rs\"\n\
         \n\
         [dependencies]\n\
         \"{user_crate_name}\" = {{ path = {user_crate_manifest_dir:?} }}\n\
         squealy-model = {model_dependency}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_paths() {
        assert_eq!(
            validate_database_path("AppDatabase").unwrap(),
            "AppDatabase"
        );
        assert_eq!(
            validate_database_path("my_crate::schemas::AppDatabase").unwrap(),
            "my_crate::schemas::AppDatabase"
        );
        assert_eq!(validate_database_path("  Db  ").unwrap(), "Db");
    }

    #[test]
    fn rejects_injection_attempts() {
        for bad in [
            "",
            "App<T>",
            "App>(); fn evil",
            "App Database",
            "App;Database",
            "1App",
            "a::-b",
            "a::",
            "::a",
        ] {
            assert!(
                validate_database_path(bad).is_err(),
                "should reject `{bad}`"
            );
        }
    }

    #[test]
    fn strips_redundant_crate_prefix() {
        assert_eq!(
            crate_relative_type_path("my_crate", "my_crate::AppDatabase"),
            "AppDatabase"
        );
        assert_eq!(
            crate_relative_type_path("my_crate", "AppDatabase"),
            "AppDatabase"
        );
        // A different leading segment is left intact (it's a module, not the crate).
        assert_eq!(
            crate_relative_type_path("my_crate", "schemas::AppDatabase"),
            "schemas::AppDatabase"
        );
    }

    #[test]
    fn stub_main_references_the_database_type() {
        let main = stub_main_rs("my_crate", "AppDatabase");
        assert!(
            main.contains("from_database::<::my_crate::AppDatabase>()"),
            "{main}"
        );
        assert!(main.contains("write_package"), "{main}");
    }

    #[test]
    fn stub_manifest_depends_on_user_crate_and_model() {
        let manifest = stub_cargo_toml("my-crate", "/abs/path", "\"0.1.0\"");
        assert!(
            manifest.contains("\"my-crate\" = { path = \"/abs/path\" }"),
            "{manifest}"
        );
        assert!(manifest.contains("squealy-model = \"0.1.0\""), "{manifest}");
    }
}
