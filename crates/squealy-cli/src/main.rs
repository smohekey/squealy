//! The `squealy` schema-management CLI.
//!
//! Commands take their model either from the crate (`--database <path>`, via a compiled stub) or from
//! a prebuilt package (`--package <file.sqz>`, which executes no project code).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use squealy_cli::extract::extract_model;
use squealy_model::{
    DatabaseModel, SchemaConnect, check_create, publish, read_package, render_create_sql,
};
use squealy_postgresql::Postgres;

#[derive(Parser)]
#[command(name = "squealy", about = "Schema management for squealy", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check whether the model is supported by the target backend.
    Check {
        #[command(flatten)]
        source: ModelSource,
    },
    /// Print create-from-scratch DDL to stdout.
    Script {
        #[command(flatten)]
        source: ModelSource,
    },
    /// Build a `.sqz` schema package from the crate.
    Export {
        /// Database type path within the crate, e.g. `AppDatabase`.
        #[arg(long)]
        database: String,
        /// Output package path.
        output: PathBuf,
    },
    /// Create the schema from scratch against a database.
    Publish {
        #[command(flatten)]
        source: ModelSource,
        /// Connection URL.
        #[arg(long)]
        url: String,
    },
}

/// Where a command's model comes from: the crate (compile + run a stub) or a prebuilt package.
#[derive(clap::Args)]
#[group(required = true, multiple = false)]
struct ModelSource {
    /// Database type path within the crate, e.g. `AppDatabase` (compiles and runs the crate).
    #[arg(long)]
    database: Option<String>,
    /// Prebuilt `.sqz` package (executes no project code).
    #[arg(long)]
    package: Option<PathBuf>,
}

impl ModelSource {
    fn load(&self) -> Result<DatabaseModel, String> {
        match (&self.database, &self.package) {
            (Some(database), None) => extract_model(database),
            (None, Some(package)) => {
                read_package(package).map_err(|error| format!("read package: {error}"))
            }
            // clap's `group(required, multiple=false)` makes the other shapes unreachable.
            _ => Err("provide exactly one of --database or --package".to_owned()),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("squealy: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Check { source } => {
            let model = source.load()?;
            check_create(&model, &Postgres).map_err(|error| format!("check model: {error}"))
        }
        Command::Script { source } => {
            let model = source.load()?;
            let sql = render_create_sql(&model, &Postgres)
                .map_err(|error| format!("render DDL: {error}"))?;
            print!("{sql}");
            Ok(())
        }
        Command::Export { database, output } => {
            let model = extract_model(&database)?;
            squealy_model::write_package(&model, &output)
                .map_err(|error| format!("write package: {error}"))
        }
        Command::Publish { source, url } => {
            let model = source.load()?;
            let mut connection = Postgres
                .connect(&url)
                .await
                .map_err(|error| format!("connect: {error}"))?;
            publish(&model, &Postgres, &mut connection)
                .await
                .map_err(|error| format!("publish: {error}"))
        }
    }
}
