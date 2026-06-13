//! Library half of the `squealy` CLI: stub generation/validation and the extract/deploy logic, kept
//! separate from `main.rs` so it can be unit-tested.

pub mod extract;
pub mod stub;

/// The CLI's error type. Failures carry a human-readable, already-contextualized (and
/// password-redacted) message; `From<String>`/`From<&str>` let the contextual `map_err`s throughout
/// the CLI flow through `?` without restating the conversion at every call site.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Message(String),
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::Message(message)
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        CliError::Message(message.to_owned())
    }
}
