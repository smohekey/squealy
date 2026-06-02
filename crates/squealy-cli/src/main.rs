//! The `squealy` schema-management CLI.
//!
//! Commands take their model either from the crate (`--database <path>`, via a compiled stub) or from
//! a prebuilt package (`--package <file.sqz>`, which executes no project code).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use squealy_cli::extract::extract_model;
use squealy_model::{
    ChangeRisk, DatabaseDiffChange, DatabaseModel, DiffPolicy, SchemaBackend, SchemaCapabilities,
    SchemaConnect, TableDiffChange, check_create, check_diff_policy, diff_models, introspect,
    publish, read_package, render_create_sql, write_package,
};
use squealy_mysql::Mysql;
use squealy_postgresql::Postgres;

#[derive(Parser)]
#[command(name = "squealy", about = "Schema management for squealy", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print backend schema-management capabilities.
    Capabilities {
        #[command(flatten)]
        backend: BackendOption,
    },
    /// Check whether the target backend can render and introspect the model metadata.
    Check {
        #[command(flatten)]
        source: ModelSource,
        #[command(flatten)]
        backend: BackendOption,
    },
    /// Print create-from-scratch DDL to stdout.
    Script {
        #[command(flatten)]
        source: ModelSource,
        #[command(flatten)]
        backend: BackendOption,
    },
    /// Build a `.sqz` schema package from the crate.
    Export {
        /// Database type path within the crate, e.g. `AppDatabase`.
        #[arg(long)]
        database: String,
        /// Output package path.
        output: PathBuf,
    },
    /// Compare two `.sqz` schema packages.
    Diff {
        /// Desired target package.
        #[arg(long)]
        desired: PathBuf,
        /// Actual/current package.
        #[arg(long)]
        actual: PathBuf,
        /// Fail when the diff contains changes blocked by policy.
        #[arg(long)]
        check_policy: bool,
        /// Allow destructive changes when checking policy.
        #[arg(long)]
        allow_destructive: bool,
        /// Allow ambiguous changes when checking policy.
        #[arg(long)]
        allow_ambiguous: bool,
    },
    /// Introspect a live database and write a `.sqz` schema package.
    Introspect {
        #[command(flatten)]
        backend: BackendOption,
        /// Connection URL.
        #[arg(long)]
        url: String,
        /// Output package path.
        output: PathBuf,
    },
    /// Create the schema from scratch against a database.
    Publish {
        #[command(flatten)]
        source: ModelSource,
        #[command(flatten)]
        backend: BackendOption,
        /// Connection URL.
        #[arg(long)]
        url: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackendKind {
    Postgres,
    Mysql,
}

/// Target SQL backend for rendering, capability checks, and publish connections.
#[derive(clap::Args)]
struct BackendOption {
    /// Target backend. Support means render plus introspection round-trip support, not just SQL syntax.
    #[arg(long, value_enum, default_value_t = BackendKind::Postgres)]
    backend: BackendKind,
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
        Command::Capabilities { backend } => {
            print_capabilities(backend.backend);
            Ok(())
        }
        Command::Check { source, backend } => {
            let model = source.load()?;
            match backend.backend {
                BackendKind::Postgres => {
                    check_create(&model, &Postgres).map_err(|error| format!("check model: {error}"))
                }
                BackendKind::Mysql => {
                    check_create(&model, &Mysql).map_err(|error| format!("check model: {error}"))
                }
            }
        }
        Command::Script { source, backend } => {
            let model = source.load()?;
            let sql = match backend.backend {
                BackendKind::Postgres => render_create_sql(&model, &Postgres),
                BackendKind::Mysql => render_create_sql(&model, &Mysql),
            }
            .map_err(|error| format!("render DDL: {error}"))?;
            print!("{sql}");
            Ok(())
        }
        Command::Export { database, output } => {
            let model = extract_model(&database)?;
            write_package(&model, &output).map_err(|error| format!("write package: {error}"))
        }
        Command::Diff {
            desired,
            actual,
            check_policy,
            allow_destructive,
            allow_ambiguous,
        } => {
            let desired =
                read_package(&desired).map_err(|error| format!("read desired: {error}"))?;
            let actual = read_package(&actual).map_err(|error| format!("read actual: {error}"))?;
            let diff = diff_models(&desired, &actual);
            print_diff(&diff);
            if check_policy {
                check_diff_policy(
                    &diff,
                    DiffPolicy {
                        allow_destructive,
                        allow_ambiguous,
                    },
                )
                .map_err(|error| format!("check diff policy: {error}"))?;
            }
            Ok(())
        }
        Command::Introspect {
            backend,
            url,
            output,
        } => match backend.backend {
            BackendKind::Postgres => {
                let mut connection = Postgres
                    .connect(&url)
                    .await
                    .map_err(|error| format!("connect: {error}"))?;
                let model = introspect(&mut connection)
                    .await
                    .map_err(|error| format!("introspect: {error}"))?;
                write_package(&model, &output).map_err(|error| format!("write package: {error}"))
            }
            BackendKind::Mysql => {
                let mut connection = Mysql
                    .connect(&url)
                    .await
                    .map_err(|error| format!("connect: {error}"))?;
                let model = introspect(&mut connection)
                    .await
                    .map_err(|error| format!("introspect: {error}"))?;
                write_package(&model, &output).map_err(|error| format!("write package: {error}"))
            }
        },
        Command::Publish {
            source,
            backend,
            url,
        } => {
            let model = source.load()?;
            match backend.backend {
                BackendKind::Postgres => {
                    let mut connection = Postgres
                        .connect(&url)
                        .await
                        .map_err(|error| format!("connect: {error}"))?;
                    publish(&model, &Postgres, &mut connection)
                        .await
                        .map_err(|error| format!("publish: {error}"))
                }
                BackendKind::Mysql => {
                    let mut connection = Mysql
                        .connect(&url)
                        .await
                        .map_err(|error| format!("connect: {error}"))?;
                    publish(&model, &Mysql, &mut connection)
                        .await
                        .map_err(|error| format!("publish: {error}"))
                }
            }
        }
    }
}

fn print_diff(diff: &squealy_model::DatabaseDiff) {
    if diff.is_empty() {
        println!("no changes");
        return;
    }

    for change in &diff.changes {
        let risk = risk_name(change.risk());
        match change {
            DatabaseDiffChange::CreateSchema { schema } => {
                println!("{risk} schema + {}", schema_name(schema));
            }
            DatabaseDiffChange::DropSchema { schema } => {
                println!("{risk} schema - {}", schema_name(schema));
            }
            DatabaseDiffChange::CreateTable { schema, table } => {
                println!("{risk} table + {}", qualified(schema, &table.name));
            }
            DatabaseDiffChange::DropTable { schema, table } => {
                println!("{risk} table - {}", qualified(schema, &table.name));
            }
            DatabaseDiffChange::AlterTable {
                schema,
                table,
                changes,
            } => {
                println!("{risk} table ~ {}", qualified(schema, table));
                for table_change in changes {
                    print_table_change(schema, table, table_change);
                }
            }
        }
    }
}

fn print_table_change(schema: &Option<String>, table: &str, change: &TableDiffChange) {
    let table = qualified(schema, table);
    let risk = risk_name(change.risk());
    match change {
        TableDiffChange::SetTableComment { .. } => println!("{risk} comment ~ {table}"),
        TableDiffChange::AddColumn { column } => {
            println!("{risk} column + {table}.{}", column.name);
        }
        TableDiffChange::DropColumn { column } => {
            println!("{risk} column - {table}.{}", column.name);
        }
        TableDiffChange::AlterColumn { after, .. } => {
            println!("{risk} column ~ {table}.{}", after.name);
        }
        TableDiffChange::AddPrimaryKey { constraint } => {
            println!("{risk} primary-key + {table}.{}", constraint.name);
        }
        TableDiffChange::DropPrimaryKey { constraint } => {
            println!("{risk} primary-key - {table}.{}", constraint.name);
        }
        TableDiffChange::AlterPrimaryKey { after, .. } => {
            println!("{risk} primary-key ~ {table}.{}", after.name);
        }
        TableDiffChange::AddUnique { constraint } => {
            println!("{risk} unique + {table}.{}", constraint.name)
        }
        TableDiffChange::DropUnique { constraint } => {
            println!("{risk} unique - {table}.{}", constraint.name);
        }
        TableDiffChange::AlterUnique { after, .. } => {
            println!("{risk} unique ~ {table}.{}", after.name);
        }
        TableDiffChange::AddForeignKey { foreign_key } => {
            println!("{risk} foreign-key + {table}.{}", foreign_key.name);
        }
        TableDiffChange::DropForeignKey { foreign_key } => {
            println!("{risk} foreign-key - {table}.{}", foreign_key.name);
        }
        TableDiffChange::AlterForeignKey { after, .. } => {
            println!("{risk} foreign-key ~ {table}.{}", after.name);
        }
        TableDiffChange::AddCheck { check } => println!("{risk} check + {table}.{}", check.name),
        TableDiffChange::DropCheck { check } => println!("{risk} check - {table}.{}", check.name),
        TableDiffChange::AlterCheck { after, .. } => {
            println!("{risk} check ~ {table}.{}", after.name);
        }
        TableDiffChange::AddIndex { index } => println!("{risk} index + {table}.{}", index.name),
        TableDiffChange::DropIndex { index } => println!("{risk} index - {table}.{}", index.name),
        TableDiffChange::AlterIndex { after, .. } => {
            println!("{risk} index ~ {table}.{}", after.name);
        }
    }
}

fn risk_name(risk: ChangeRisk) -> &'static str {
    match risk {
        ChangeRisk::Safe => "safe",
        ChangeRisk::Destructive => "destructive",
        ChangeRisk::Ambiguous => "ambiguous",
    }
}

fn qualified(schema: &Option<String>, name: &str) -> String {
    format!("{}.{}", schema_name(schema), name)
}

fn schema_name(schema: &Option<String>) -> &str {
    schema.as_deref().unwrap_or("<default>")
}

fn print_capabilities(backend: BackendKind) {
    let capabilities = match backend {
        BackendKind::Postgres => Postgres.capabilities(),
        BackendKind::Mysql => Mysql.capabilities(),
    };

    println!("backend={}", backend.value_name());
    print_schema_capabilities(capabilities);
}

fn print_schema_capabilities(capabilities: SchemaCapabilities) {
    println!(
        "constraints.foreign_key_match_type={}",
        capabilities.constraints.foreign_key_match_type
    );
    println!(
        "constraints.foreign_key_deferrability={}",
        capabilities.constraints.foreign_key_deferrability
    );
    println!(
        "constraints.foreign_key_validation={}",
        capabilities.constraints.foreign_key_validation
    );
    println!(
        "constraints.foreign_key_enforcement={}",
        capabilities.constraints.foreign_key_enforcement
    );
    println!(
        "constraints.check_validation={}",
        capabilities.constraints.check_validation
    );
    println!(
        "constraints.check_enforcement={}",
        capabilities.constraints.check_enforcement
    );
    println!("indexes.predicates={}", capabilities.indexes.predicates);
    println!("indexes.expressions={}", capabilities.indexes.expressions);
    println!(
        "indexes.include_columns={}",
        capabilities.indexes.include_columns
    );
    println!(
        "indexes.null_ordering={}",
        capabilities.indexes.null_ordering
    );
    println!("indexes.collations={}", capabilities.indexes.collations);
    println!(
        "indexes.operator_classes={}",
        capabilities.indexes.operator_classes
    );
}

impl BackendKind {
    fn value_name(self) -> &'static str {
        match self {
            BackendKind::Postgres => "postgres",
            BackendKind::Mysql => "mysql",
        }
    }
}
