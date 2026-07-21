# squealy

`squealy` is a multi-crate Rust workspace.

## Crates

- `squealy`: the primary executable
- `squealy-core`: shared domain functionality

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```
