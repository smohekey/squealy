//! The `squealy` schema-management CLI.
//!
//! Commands take their model either from the crate (`--database <path>`, via a compiled stub) or from
//! a prebuilt package (`--package <file.sqz>`, which executes no project code).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use squealy_cli::extract::extract_model;
use squealy_model::{
    ChangeRisk, DatabaseDiffChange, DatabaseModel, DiffPolicy, RefactorLog, SchemaBackend,
    SchemaCapabilities, SchemaConnect, TableDiffChange, apply_plan, check_create,
    check_diff_policy, diff_models, introspect, plan_from_database_with_refactors,
    plan_models_with_refactors, publish, read_package, read_refactor_log, refactor_from_kdl,
    render_create_sql, render_plan_sql, write_package, write_package_with_refactors,
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
        /// Optional refactor.kdl file to embed in the output package.
        #[arg(long)]
        refactors: Option<PathBuf>,
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
    /// Render an incremental DDL plan between two `.sqz` schema packages.
    Plan {
        /// Desired target package.
        #[arg(long)]
        desired: PathBuf,
        /// Actual/current package.
        #[arg(long)]
        actual: PathBuf,
        #[command(flatten)]
        backend: BackendOption,
        /// Allow destructive changes when planning.
        #[arg(long)]
        allow_destructive: bool,
        /// Allow ambiguous changes when planning.
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
    /// Publish schema changes against a database.
    Publish {
        #[command(flatten)]
        source: ModelSource,
        #[command(flatten)]
        backend: BackendOption,
        /// Connection URL.
        #[arg(long)]
        url: String,
        /// Introspect the live database and apply an incremental plan instead of create-from-scratch DDL.
        #[arg(long)]
        incremental: bool,
        /// Print the incremental plan SQL instead of executing it.
        #[arg(long)]
        report: bool,
        /// Allow destructive changes when publishing incrementally.
        #[arg(long)]
        allow_destructive: bool,
        /// Allow ambiguous changes when publishing incrementally.
        #[arg(long)]
        allow_ambiguous: bool,
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
        Ok(self.load_with_refactors()?.model)
    }

    fn load_with_refactors(&self) -> Result<LoadedModel, String> {
        match (&self.database, &self.package) {
            (Some(database), None) => Ok(LoadedModel {
                model: extract_model(database)?,
                refactors: RefactorLog::default(),
            }),
            (None, Some(package)) => {
                let model =
                    read_package(package).map_err(|error| format!("read package: {error}"))?;
                let refactors = read_refactor_log(package)
                    .map_err(|error| format!("read package refactors: {error}"))?;
                Ok(LoadedModel { model, refactors })
            }
            // clap's `group(required, multiple=false)` makes the other shapes unreachable.
            _ => Err("provide exactly one of --database or --package".to_owned()),
        }
    }
}

struct LoadedModel {
    model: DatabaseModel,
    refactors: RefactorLog,
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
        Command::Export {
            database,
            refactors,
            output,
        } => {
            let model = extract_model(&database)?;
            if let Some(refactors) = refactors {
                let refactors = read_refactor_file(&refactors)?;
                write_package_with_refactors(&model, &refactors, &output)
                    .map_err(|error| format!("write package: {error}"))
            } else {
                write_package(&model, &output).map_err(|error| format!("write package: {error}"))
            }
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
        Command::Plan {
            desired,
            actual,
            backend,
            allow_destructive,
            allow_ambiguous,
        } => {
            let refactors = read_refactor_log(&desired)
                .map_err(|error| format!("read desired refactors: {error}"))?;
            let desired =
                read_package(&desired).map_err(|error| format!("read desired: {error}"))?;
            let actual = read_package(&actual).map_err(|error| format!("read actual: {error}"))?;
            let policy = DiffPolicy {
                allow_destructive,
                allow_ambiguous,
            };
            let plan = plan_models_with_refactors(&desired, &actual, &refactors, policy)
                .map_err(|error| format!("plan schema changes: {error}"))?;
            let sql = match backend.backend {
                BackendKind::Postgres => render_plan_sql(&plan, &Postgres),
                BackendKind::Mysql => render_plan_sql(&plan, &Mysql),
            }
            .map_err(|error| format!("render plan: {error}"))?;
            print!("{sql}");
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
            incremental,
            report,
            allow_destructive,
            allow_ambiguous,
        } => {
            if report && !incremental {
                return Err("--report currently requires --incremental".to_owned());
            }
            let loaded = source.load_with_refactors()?;
            match backend.backend {
                BackendKind::Postgres => {
                    let mut connection = Postgres
                        .connect(&url)
                        .await
                        .map_err(|error| format!("connect: {error}"))?;
                    if incremental {
                        check_create(&loaded.model, &Postgres)
                            .map_err(|error| format!("check model: {error}"))?;
                        let plan = plan_from_database_with_refactors(
                            &loaded.model,
                            &loaded.refactors,
                            &mut connection,
                            DiffPolicy {
                                allow_destructive,
                                allow_ambiguous,
                            },
                        )
                        .await
                        .map_err(|error| format!("plan: {error}"))?;
                        if report {
                            let sql = render_plan_sql(&plan, &Postgres)
                                .map_err(|error| format!("render plan: {error}"))?;
                            print!("{sql}");
                            Ok(())
                        } else {
                            apply_plan(&plan, &Postgres, &mut connection)
                                .await
                                .map_err(|error| format!("publish: {error}"))
                        }
                    } else {
                        publish(&loaded.model, &Postgres, &mut connection)
                            .await
                            .map_err(|error| format!("publish: {error}"))
                    }
                }
                BackendKind::Mysql => {
                    let mut connection = Mysql
                        .connect(&url)
                        .await
                        .map_err(|error| format!("connect: {error}"))?;
                    if incremental {
                        check_create(&loaded.model, &Mysql)
                            .map_err(|error| format!("check model: {error}"))?;
                        let plan = plan_from_database_with_refactors(
                            &loaded.model,
                            &loaded.refactors,
                            &mut connection,
                            DiffPolicy {
                                allow_destructive,
                                allow_ambiguous,
                            },
                        )
                        .await
                        .map_err(|error| format!("plan: {error}"))?;
                        if report {
                            let sql = render_plan_sql(&plan, &Mysql)
                                .map_err(|error| format!("render plan: {error}"))?;
                            print!("{sql}");
                            Ok(())
                        } else {
                            apply_plan(&plan, &Mysql, &mut connection)
                                .await
                                .map_err(|error| format!("publish: {error}"))
                        }
                    } else {
                        publish(&loaded.model, &Mysql, &mut connection)
                            .await
                            .map_err(|error| format!("publish: {error}"))
                    }
                }
            }
        }
    }
}

fn read_refactor_file(path: &Path) -> Result<RefactorLog, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("read refactor file: {error}"))?;
    refactor_from_kdl(&text).map_err(|error| format!("parse refactor file: {error}"))
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
