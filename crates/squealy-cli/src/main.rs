//! The `squealy` schema-management CLI.
//!
//! Commands take their model either from the crate (`--database <path>`, via a compiled stub) or from
//! a prebuilt package (`--package <file.sqz>`, which executes no project code).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use squealy_cli::extract::extract_model;
use squealy_model::{
    ChangeRisk, DatabaseDiffChange, DatabaseModel, DiffPolicy, RefactorLog, SchemaBackend,
    SchemaCapabilities, SchemaConnect, SchemaMetadataStore, SchemaPublishHistoryStore,
    SchemaPublishRecord, SchemaRefactorStore, TableDiffChange, apply_plan, check_create,
    check_diff_policy, diff_models, introspect, package_metadata, pending_refactors,
    plan_from_database_with_refactors, plan_models_with_refactors, publish, read_package,
    read_refactor_log, refactor_from_kdl, render_create_sql, render_plan_sql,
    repair_refactor_metadata, write_package, write_package_with_refactors,
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
    /// Compare a desired model with live database state without applying changes.
    Status {
        #[command(flatten)]
        source: ModelSource,
        #[command(flatten)]
        backend: BackendOption,
        /// Connection URL.
        #[arg(long)]
        url: String,
    },
    /// Inspect or repair schema refactor metadata recorded in a live database.
    Refactors {
        #[command(subcommand)]
        command: RefactorsCommand,
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

#[derive(Subcommand)]
enum RefactorsCommand {
    /// Print applied schema refactors recorded in a live database.
    List {
        #[command(flatten)]
        backend: BackendOption,
        /// Connection URL.
        #[arg(long)]
        url: String,
        /// Optional package whose refactor log should be compared with the recorded ids.
        #[arg(long)]
        package: Option<PathBuf>,
    },
    /// Record missing refactor ids after validating that the live schema already reflects them.
    Repair {
        #[command(flatten)]
        backend: BackendOption,
        /// Connection URL.
        #[arg(long)]
        url: String,
        /// Package whose refactor log should be validated and recorded.
        #[arg(long)]
        package: PathBuf,
    },
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
        Command::Status {
            source,
            backend,
            url,
        } => {
            let loaded = source.load_with_refactors()?;
            check_model_for_backend(&loaded.model, backend.backend)?;
            let (actual, applied_ids, live_metadata, publish_history) =
                live_status_inputs(backend.backend, &url).await?;
            pending_refactors(&loaded.refactors, &applied_ids, &actual)
                .map_err(|error| format!("applied refactor metadata mismatch: {error}"))?;
            let desired_metadata = package_metadata(&loaded.model, &loaded.refactors);
            print_status(
                &loaded.model,
                &actual,
                &loaded.refactors,
                &applied_ids,
                &desired_metadata,
                &live_metadata,
                &publish_history,
            );
            Ok(())
        }
        Command::Refactors { command } => run_refactors(command).await,
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
                                .map_err(|error| format!("publish: {error}"))?;
                            record_publish_metadata(&loaded, "incremental", &mut connection).await
                        }
                    } else {
                        publish(&loaded.model, &Postgres, &mut connection)
                            .await
                            .map_err(|error| format!("publish: {error}"))?;
                        record_publish_metadata(&loaded, "create", &mut connection).await
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
                                .map_err(|error| format!("publish: {error}"))?;
                            record_publish_metadata(&loaded, "incremental", &mut connection).await
                        }
                    } else {
                        publish(&loaded.model, &Mysql, &mut connection)
                            .await
                            .map_err(|error| format!("publish: {error}"))?;
                        record_publish_metadata(&loaded, "create", &mut connection).await
                    }
                }
            }
        }
    }
}

fn check_model_for_backend(model: &DatabaseModel, backend: BackendKind) -> Result<(), String> {
    match backend {
        BackendKind::Postgres => {
            check_create(model, &Postgres).map_err(|error| format!("check model: {error}"))
        }
        BackendKind::Mysql => {
            check_create(model, &Mysql).map_err(|error| format!("check model: {error}"))
        }
    }
}

async fn live_status_inputs(
    backend: BackendKind,
    url: &str,
) -> Result<
    (
        DatabaseModel,
        Vec<String>,
        Vec<(String, String)>,
        Vec<SchemaPublishRecord>,
    ),
    String,
> {
    match backend {
        BackendKind::Postgres => {
            let mut connection = Postgres
                .connect(url)
                .await
                .map_err(|error| format!("connect: {error}"))?;
            let actual = introspect(&mut connection)
                .await
                .map_err(|error| format!("introspect: {error}"))?;
            let applied_ids = connection
                .applied_refactor_ids()
                .await
                .map_err(|error| format!("read applied refactors: {error}"))?;
            let metadata = connection
                .schema_metadata()
                .await
                .map_err(|error| format!("read schema metadata: {error}"))?;
            let publish_history = connection
                .schema_publish_history(1)
                .await
                .map_err(|error| format!("read publish history: {error}"))?;
            Ok((actual, applied_ids, metadata, publish_history))
        }
        BackendKind::Mysql => {
            let mut connection = Mysql
                .connect(url)
                .await
                .map_err(|error| format!("connect: {error}"))?;
            let actual = introspect(&mut connection)
                .await
                .map_err(|error| format!("introspect: {error}"))?;
            let applied_ids = connection
                .applied_refactor_ids()
                .await
                .map_err(|error| format!("read applied refactors: {error}"))?;
            let metadata = connection
                .schema_metadata()
                .await
                .map_err(|error| format!("read schema metadata: {error}"))?;
            let publish_history = connection
                .schema_publish_history(1)
                .await
                .map_err(|error| format!("read publish history: {error}"))?;
            Ok((actual, applied_ids, metadata, publish_history))
        }
    }
}

async fn record_publish_metadata<C>(
    loaded: &LoadedModel,
    mode: &str,
    connection: &mut C,
) -> Result<(), String>
where
    C: SchemaMetadataStore + SchemaPublishHistoryStore<Error = <C as SchemaMetadataStore>::Error>,
    <C as SchemaMetadataStore>::Error: std::fmt::Display,
{
    let metadata = package_metadata(&loaded.model, &loaded.refactors);
    let package_hash = metadata_value(&metadata, "package.content_hash")?;
    let package_format_version = metadata_value(&metadata, "package.format_version")?;
    connection
        .record_schema_metadata(&metadata)
        .await
        .map_err(|error| format!("record schema metadata: {error}"))?;
    connection
        .record_schema_publish(mode, package_hash, package_format_version)
        .await
        .map_err(|error| format!("record publish history: {error}"))
}

fn metadata_value<'metadata>(
    metadata: &'metadata [(String, String)],
    key: &str,
) -> Result<&'metadata str, String> {
    metadata
        .iter()
        .find(|(metadata_key, _)| metadata_key == key)
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| format!("missing package metadata key `{key}`"))
}

async fn run_refactors(command: RefactorsCommand) -> Result<(), String> {
    match command {
        RefactorsCommand::List {
            backend,
            url,
            package,
        } => {
            let applied_ids = applied_refactor_ids(backend.backend, &url).await?;
            if let Some(package) = package {
                let refactors = read_refactor_log(&package)
                    .map_err(|error| format!("read package refactors: {error}"))?;
                print_refactor_status(&refactors, &applied_ids);
            } else {
                print_applied_refactors(&applied_ids);
            }
            Ok(())
        }
        RefactorsCommand::Repair {
            backend,
            url,
            package,
        } => {
            let refactors = read_refactor_log(&package)
                .map_err(|error| format!("read package refactors: {error}"))?;
            match backend.backend {
                BackendKind::Postgres => {
                    let mut connection = Postgres
                        .connect(&url)
                        .await
                        .map_err(|error| format!("connect: {error}"))?;
                    let report = repair_refactor_metadata(&refactors, &mut connection)
                        .await
                        .map_err(|error| format!("repair refactor metadata: {error}"))?;
                    print_refactor_repair_report(&report);
                    Ok(())
                }
                BackendKind::Mysql => {
                    let mut connection = Mysql
                        .connect(&url)
                        .await
                        .map_err(|error| format!("connect: {error}"))?;
                    let report = repair_refactor_metadata(&refactors, &mut connection)
                        .await
                        .map_err(|error| format!("repair refactor metadata: {error}"))?;
                    print_refactor_repair_report(&report);
                    Ok(())
                }
            }
        }
    }
}

async fn applied_refactor_ids(backend: BackendKind, url: &str) -> Result<Vec<String>, String> {
    match backend {
        BackendKind::Postgres => {
            let mut connection = Postgres
                .connect(url)
                .await
                .map_err(|error| format!("connect: {error}"))?;
            connection
                .applied_refactor_ids()
                .await
                .map_err(|error| format!("read applied refactors: {error}"))
        }
        BackendKind::Mysql => {
            let mut connection = Mysql
                .connect(url)
                .await
                .map_err(|error| format!("connect: {error}"))?;
            connection
                .applied_refactor_ids()
                .await
                .map_err(|error| format!("read applied refactors: {error}"))
        }
    }
}

fn read_refactor_file(path: &Path) -> Result<RefactorLog, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("read refactor file: {error}"))?;
    refactor_from_kdl(&text).map_err(|error| format!("parse refactor file: {error}"))
}

fn print_applied_refactors(applied_ids: &[String]) {
    if applied_ids.is_empty() {
        println!("no applied refactors");
        return;
    }

    for id in applied_ids {
        println!("applied {id}");
    }
}

fn print_refactor_status(refactors: &RefactorLog, applied_ids: &[String]) {
    let applied = applied_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let package_ids = refactors
        .operations
        .iter()
        .map(|operation| operation.id())
        .collect::<BTreeSet<_>>();

    for operation in &refactors.operations {
        let id = operation.id();
        if applied.contains(id) {
            println!("applied {id}");
        } else {
            println!("pending {id}");
        }
    }

    for id in applied.difference(&package_ids) {
        println!("recorded-only {id}");
    }

    if refactors.is_empty() && applied_ids.is_empty() {
        println!("no refactors");
    }
}

fn print_refactor_repair_report(report: &squealy_model::RefactorRepairReport) {
    if report.recorded.is_empty() && report.already_recorded.is_empty() {
        println!("no refactors");
        return;
    }

    for id in &report.recorded {
        println!("recorded {id}");
    }

    for id in &report.already_recorded {
        println!("already-recorded {id}");
    }
}

fn print_status(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
    refactors: &RefactorLog,
    applied_ids: &[String],
    desired_metadata: &[(String, String)],
    live_metadata: &[(String, String)],
    publish_history: &[SchemaPublishRecord],
) {
    let diff = diff_models(desired, actual);
    if diff.is_empty() {
        println!("schema clean");
    } else {
        println!("schema changes");
        print_diff(&diff);
    }

    print_refactor_status(refactors, applied_ids);
    print_metadata_status(desired_metadata, live_metadata);
    print_publish_history_status(publish_history);
}

fn print_metadata_status(
    desired_metadata: &[(String, String)],
    live_metadata: &[(String, String)],
) {
    let live = live_metadata
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<BTreeSet<_>>();
    let live_keys = live_metadata
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<BTreeSet<_>>();

    for (key, desired_value) in desired_metadata {
        let status = if live.contains(&(key.as_str(), desired_value.as_str())) {
            "match"
        } else if live_keys.contains(key.as_str()) {
            "mismatch"
        } else {
            "missing"
        };
        println!("metadata {key} {status}");
    }
}

fn print_publish_history_status(publish_history: &[SchemaPublishRecord]) {
    let Some(latest) = publish_history.first() else {
        println!("publish-history none");
        return;
    };

    println!(
        "publish-history latest mode={} package.content_hash={} package.format_version={} applied_at={}",
        latest.mode, latest.package_hash, latest.package_format_version, latest.applied_at
    );
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
