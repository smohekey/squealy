//! The `squealy` schema-management CLI.
//!
//! Commands take their model either from the crate (`--database <path>`, via a compiled stub) or from
//! a prebuilt package (`--package <file.sqz>`, which executes no project code).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use squealy_cli::CliError;
use squealy_cli::extract::extract_model;
use squealy_model::{
    ChangeRisk, ClassifiedDatabaseDiffChange, DatabaseDiffChange, DatabaseModel, DatabasePlan,
    DatabasePlanStep, DdlExecutor, DiffPolicy, PlanApplyOptions, PlanFromDatabaseError,
    RefactorLog, SchemaBackend, SchemaCapabilities, SchemaConnect, SchemaMetadataStore,
    SchemaPublishHistoryStore, SchemaPublishRecord, SchemaRefactorStore, TableDiffChange,
    TablePlanStep, apply_plan_with_options, canonicalize_model, check_create, check_diff_policy,
    classified_plan_steps, diff_models, introspect, package_metadata, pending_refactors,
    plan_from_database_with_refactors, plan_models_with_refactors, publish, read_package,
    read_refactor_log, refactor_from_kdl, render_create_sql, render_plan_sql,
    render_plan_with_options, repair_refactor_metadata, write_package,
    write_package_with_refactors,
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
    /// Compare two schema models, each from a crate database type or a `.sqz` package.
    Diff {
        /// Desired model from a crate database type (compiles and runs the crate).
        #[arg(long = "desired-database", conflicts_with = "desired")]
        desired_database: Option<String>,
        /// Desired model from a `.sqz` package.
        #[arg(long)]
        desired: Option<PathBuf>,
        /// Actual model from a crate database type.
        #[arg(long = "actual-database", conflicts_with = "actual")]
        actual_database: Option<String>,
        /// Actual model from a `.sqz` package.
        #[arg(long)]
        actual: Option<PathBuf>,
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
    /// Render an incremental DDL plan between two schema models (crate database type or `.sqz`).
    Plan {
        /// Desired model from a crate database type (compiles and runs the crate).
        #[arg(long = "desired-database", conflicts_with = "desired")]
        desired_database: Option<String>,
        /// Desired model from a `.sqz` package.
        #[arg(long)]
        desired: Option<PathBuf>,
        /// refactor.kdl applied to the desired side when it comes from a crate (a package carries
        /// its own embedded refactor log).
        #[arg(long)]
        refactors: Option<PathBuf>,
        /// Actual model from a crate database type.
        #[arg(long = "actual-database", conflicts_with = "actual")]
        actual_database: Option<String>,
        /// Actual model from a `.sqz` package.
        #[arg(long)]
        actual: Option<PathBuf>,
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
        /// Number of recent publish history rows to print.
        #[arg(long, default_value_t = 1)]
        history: usize,
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Exit non-zero when live schema differs from the desired model.
        #[arg(long)]
        check_schema: bool,
        /// Exit non-zero when package refactors are pending or recorded ids are outside the package.
        #[arg(long)]
        check_refactors: bool,
        /// Exit non-zero when package metadata is missing or mismatched.
        #[arg(long)]
        check_metadata: bool,
        /// Enable all status checks.
        #[arg(long)]
        check_all: bool,
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
        /// Abort if a required lock cannot be acquired within this many seconds (sets Postgres
        /// `lock_timeout` / MySQL `lock_wait_timeout`). Recommended in production so a publish fails
        /// fast instead of blocking — and queuing other queries — behind a held lock.
        #[arg(long)]
        lock_timeout: Option<u64>,
        /// Abort any single statement that runs longer than this many seconds (Postgres
        /// `statement_timeout`; ignored by MySQL, which has no DDL statement timeout).
        #[arg(long)]
        statement_timeout: Option<u64>,
        /// Apply destructive changes without the interactive confirmation prompt (required to apply
        /// destructive changes when stdin is not a terminal, e.g. in CI).
        #[arg(long)]
        yes: bool,
        /// Create new indexes concurrently, outside the transaction (PostgreSQL `CREATE INDEX
        /// CONCURRENTLY`), so the table is not locked against writes while they build. Trades the
        /// atomic all-or-nothing apply for non-blocking index creation. Only affects `--incremental`.
        #[arg(long)]
        concurrent_indexes: bool,
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
    fn load(&self) -> Result<DatabaseModel, CliError> {
        Ok(self.load_with_refactors()?.model)
    }

    fn load_with_refactors(&self) -> Result<LoadedModel, CliError> {
        match (&self.database, &self.package) {
            (Some(database), None) => Ok(LoadedModel {
                model: extract_model(database)?,
                refactors: RefactorLog::default(),
            }),
            (None, Some(package)) => {
                let model = read_package(package)
                    .map_err(|error| CliError::Message(format!("read package: {error}")))?;
                let refactors = read_refactor_log(package).map_err(|error| {
                    CliError::Message(format!("read package refactors: {error}"))
                })?;
                Ok(LoadedModel { model, refactors })
            }
            // clap's `group(required, multiple=false)` makes the other shapes unreachable.
            _ => Err(CliError::Message(
                "provide exactly one of --database or --package".to_owned(),
            )),
        }
    }
}

struct LoadedModel {
    model: DatabaseModel,
    refactors: RefactorLog,
}

struct StatusChecks {
    schema: bool,
    refactors: bool,
    metadata: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("squealy: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Installs the `tracing` subscriber that emits diagnostics and operational events to stderr.
///
/// Verbosity follows `RUST_LOG` (default: `warn`), so normal runs stay quiet and stdout — where the
/// commands write their actual output (SQL, JSON, reports) — is never touched. All output passes
/// through [`RedactingMakeWriter`] so a credential can never reach the logs.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_writer(RedactingMakeWriter)
        .with_target(false)
        .without_time()
        .init();
}

/// A `MakeWriter` that scrubs connection-URL passwords from every log line before it reaches stderr,
/// as a defense-in-depth backstop on top of redacting at the source.
struct RedactingMakeWriter;

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for RedactingMakeWriter {
    type Writer = RedactingWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        RedactingWriter
    }
}

struct RedactingWriter;

impl std::io::Write for RedactingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let redacted = redact_credentials(&String::from_utf8_lossy(buf));
        std::io::stderr().write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// Replaces the password in any `scheme://user:password@host` URL found in `text` with `***`.
fn redact_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find("://") {
        let (before, after) = rest.split_at(position + 3);
        out.push_str(before);
        let authority_end = after
            .find(['/', '?', '#', ' ', '"', '\'', '\n'])
            .unwrap_or(after.len());
        let authority = &after[..authority_end];
        // Split on the *last* `@` so a password that itself contains `@` is fully masked.
        match authority.rsplit_once('@') {
            Some((userinfo, host)) if userinfo.contains(':') => {
                let (user, _password) = userinfo.split_once(':').expect("contains ':'");
                out.push_str(user);
                out.push_str(":***@");
                out.push_str(host);
            }
            _ => out.push_str(authority),
        }
        rest = &after[authority_end..];
    }
    out.push_str(rest);
    out
}

async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Command::Capabilities { backend } => {
            print_capabilities(backend.backend);
            Ok(())
        }
        Command::Check { source, backend } => {
            let model = source.load()?;
            match backend.backend {
                BackendKind::Postgres => check_create(&model, &Postgres)
                    .map_err(|error| CliError::Message(format!("check model: {error}"))),
                BackendKind::Mysql => check_create(&model, &Mysql)
                    .map_err(|error| CliError::Message(format!("check model: {error}"))),
            }
        }
        Command::Script { source, backend } => {
            let model = source.load()?;
            let sql = match backend.backend {
                BackendKind::Postgres => render_create_sql(&model, &Postgres),
                BackendKind::Mysql => render_create_sql(&model, &Mysql),
            }
            .map_err(|error| CliError::Message(format!("render DDL: {error}")))?;
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
                    .map_err(|error| CliError::Message(format!("write package: {error}")))
            } else {
                write_package(&model, &output)
                    .map_err(|error| CliError::Message(format!("write package: {error}")))
            }
        }
        Command::Diff {
            desired_database,
            desired,
            actual_database,
            actual,
            check_policy,
            allow_destructive,
            allow_ambiguous,
        } => {
            let desired = load_model(desired_database, desired, "desired")?;
            let actual = load_model(actual_database, actual, "actual")?;
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
                .map_err(|error| policy_blocked_error(&error.blocked))?;
            }
            Ok(())
        }
        Command::Plan {
            desired_database,
            desired,
            refactors,
            actual_database,
            actual,
            backend,
            allow_destructive,
            allow_ambiguous,
        } => {
            let desired = load_desired_model(desired_database, desired, refactors)?;
            let actual = load_model(actual_database, actual, "actual")?;
            let policy = DiffPolicy {
                allow_destructive,
                allow_ambiguous,
            };
            let plan =
                plan_models_with_refactors(&desired.model, &actual, &desired.refactors, policy)
                    .map_err(|error| policy_blocked_error(&error.blocked))?;
            let sql = match backend.backend {
                BackendKind::Postgres => render_plan_sql(&plan, &Postgres),
                BackendKind::Mysql => render_plan_sql(&plan, &Mysql),
            }
            .map_err(|error| CliError::Message(format!("render plan: {error}")))?;
            print!("{sql}");
            Ok(())
        }
        Command::Introspect {
            backend,
            url,
            output,
        } => {
            validate_url(backend.backend, &url)?;
            match backend.backend {
                BackendKind::Postgres => {
                    let mut connection = Postgres.connect(&url).await.map_err(|error| {
                        CliError::Message(format!(
                            "connect: {}",
                            redact_secret(&error.to_string(), url)
                        ))
                    })?;
                    let model = introspect(&mut connection)
                        .await
                        .map_err(|error| CliError::Message(format!("introspect: {error}")))?;
                    write_package(&model, &output)
                        .map_err(|error| CliError::Message(format!("write package: {error}")))
                }
                BackendKind::Mysql => {
                    let mut connection = Mysql.connect(&url).await.map_err(|error| {
                        CliError::Message(format!(
                            "connect: {}",
                            redact_secret(&error.to_string(), url)
                        ))
                    })?;
                    let model = introspect(&mut connection)
                        .await
                        .map_err(|error| CliError::Message(format!("introspect: {error}")))?;
                    write_package(&model, &output)
                        .map_err(|error| CliError::Message(format!("write package: {error}")))
                }
            }
        }
        Command::Status {
            source,
            backend,
            url,
            history,
            json,
            check_schema,
            check_refactors,
            check_metadata,
            check_all,
        } => {
            let loaded = source.load_with_refactors()?;
            check_model_for_backend(&loaded.model, backend.backend)?;
            let (desired, actual, applied_ids, live_metadata, publish_history) =
                live_status_inputs(backend.backend, &url, history, &loaded.model).await?;
            pending_refactors(&loaded.refactors, &applied_ids, &actual).map_err(|error| {
                CliError::Message(format!("applied refactor metadata mismatch: {error}"))
            })?;
            let desired_metadata = package_metadata(&loaded.model, &loaded.refactors);
            if json {
                print_status_json(
                    &desired,
                    &actual,
                    &loaded.refactors,
                    &applied_ids,
                    &desired_metadata,
                    &live_metadata,
                    &publish_history,
                )?;
            } else {
                print_status(
                    &desired,
                    &actual,
                    &loaded.refactors,
                    &applied_ids,
                    &desired_metadata,
                    &live_metadata,
                    &publish_history,
                );
            }
            let checks = StatusChecks {
                schema: check_all || check_schema,
                refactors: check_all || check_refactors,
                metadata: check_all || check_metadata,
            };
            check_status(
                &checks,
                &desired,
                &actual,
                &loaded.refactors,
                &applied_ids,
                &desired_metadata,
                &live_metadata,
            )?;
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
            lock_timeout,
            statement_timeout,
            yes,
            concurrent_indexes,
        } => {
            let apply_options = PlanApplyOptions { concurrent_indexes };
            let loaded = source.load_with_refactors()?;
            // Create-from-scratch dry-run: render the DDL without touching a database.
            if report && !incremental {
                let sql = match backend.backend {
                    BackendKind::Postgres => render_create_sql(&loaded.model, &Postgres),
                    BackendKind::Mysql => render_create_sql(&loaded.model, &Mysql),
                }
                .map_err(|error| CliError::Message(format!("render DDL: {error}")))?;
                print!("{sql}");
                return Ok(());
            }
            validate_url(backend.backend, &url)?;
            tracing::info!(
                backend = backend.backend.value_name(),
                mode = if incremental { "incremental" } else { "create" },
                report,
                "publishing schema"
            );
            match backend.backend {
                BackendKind::Postgres => {
                    let mut connection = Postgres.connect(&url).await.map_err(|error| {
                        CliError::Message(format!(
                            "connect: {}",
                            redact_secret(&error.to_string(), url)
                        ))
                    })?;
                    if !report {
                        apply_session_timeouts(
                            &mut connection,
                            BackendKind::Postgres,
                            lock_timeout,
                            statement_timeout,
                        )
                        .await?;
                    }
                    if incremental {
                        check_create(&loaded.model, &Postgres)
                            .map_err(|error| CliError::Message(format!("check model: {error}")))?;
                        let plan = plan_from_database_result(
                            plan_from_database_with_refactors(
                                &loaded.model,
                                &loaded.refactors,
                                &mut connection,
                                DiffPolicy {
                                    allow_destructive,
                                    allow_ambiguous,
                                },
                            )
                            .await,
                        )?;
                        if report {
                            let sql = render_plan_with_options(&plan, &Postgres, apply_options)
                                .map_err(|error| {
                                    CliError::Message(format!("render plan: {error}"))
                                })?;
                            print!("{sql}");
                            Ok(())
                        } else {
                            confirm_destructive(&plan, yes)?;
                            apply_plan_with_options(
                                &plan,
                                &Postgres,
                                &mut connection,
                                apply_options,
                            )
                            .await
                            .map_err(|error| CliError::Message(format!("publish: {error}")))?;
                            record_publish_metadata(&loaded, "incremental", &mut connection).await
                        }
                    } else {
                        publish(&loaded.model, &Postgres, &mut connection)
                            .await
                            .map_err(|error| CliError::Message(format!("publish: {error}")))?;
                        record_publish_metadata(&loaded, "create", &mut connection).await
                    }
                }
                BackendKind::Mysql => {
                    let mut connection = Mysql.connect(&url).await.map_err(|error| {
                        CliError::Message(format!(
                            "connect: {}",
                            redact_secret(&error.to_string(), url)
                        ))
                    })?;
                    if !report {
                        apply_session_timeouts(
                            &mut connection,
                            BackendKind::Mysql,
                            lock_timeout,
                            statement_timeout,
                        )
                        .await?;
                    }
                    if incremental {
                        check_create(&loaded.model, &Mysql)
                            .map_err(|error| CliError::Message(format!("check model: {error}")))?;
                        let plan = plan_from_database_result(
                            plan_from_database_with_refactors(
                                &loaded.model,
                                &loaded.refactors,
                                &mut connection,
                                DiffPolicy {
                                    allow_destructive,
                                    allow_ambiguous,
                                },
                            )
                            .await,
                        )?;
                        if report {
                            let sql = render_plan_with_options(&plan, &Mysql, apply_options)
                                .map_err(|error| {
                                    CliError::Message(format!("render plan: {error}"))
                                })?;
                            print!("{sql}");
                            Ok(())
                        } else {
                            confirm_destructive(&plan, yes)?;
                            apply_plan_with_options(&plan, &Mysql, &mut connection, apply_options)
                                .await
                                .map_err(|error| CliError::Message(format!("publish: {error}")))?;
                            record_publish_metadata(&loaded, "incremental", &mut connection).await
                        }
                    } else {
                        publish(&loaded.model, &Mysql, &mut connection)
                            .await
                            .map_err(|error| CliError::Message(format!("publish: {error}")))?;
                        record_publish_metadata(&loaded, "create", &mut connection).await
                    }
                }
            }
        }
    }
}

fn check_model_for_backend(model: &DatabaseModel, backend: BackendKind) -> Result<(), CliError> {
    match backend {
        BackendKind::Postgres => check_create(model, &Postgres)
            .map_err(|error| CliError::Message(format!("check model: {error}"))),
        BackendKind::Mysql => check_create(model, &Mysql)
            .map_err(|error| CliError::Message(format!("check model: {error}"))),
    }
}

/// Introspects the live database and returns the inputs the status command diffs. The returned
/// desired model is canonicalized through the live backend (identically to [`plan_from_database`]),
/// so `status` does not report spurious schema drift for backend-equivalent metadata immediately
/// after a successful publish.
async fn live_status_inputs(
    backend: BackendKind,
    url: &str,
    history: usize,
    desired: &DatabaseModel,
) -> Result<
    (
        DatabaseModel,
        DatabaseModel,
        Vec<String>,
        Vec<(String, String)>,
        Vec<SchemaPublishRecord>,
    ),
    CliError,
> {
    validate_url(backend, url)?;
    match backend {
        BackendKind::Postgres => {
            let mut connection = Postgres.connect(url).await.map_err(|error| {
                CliError::Message(format!(
                    "connect: {}",
                    redact_secret(&error.to_string(), url)
                ))
            })?;
            let actual = introspect(&mut connection)
                .await
                .map_err(|error| CliError::Message(format!("introspect: {error}")))?;
            // Canonicalize both sides so equivalent predicate / CHECK expressions compare equal.
            let actual = canonicalize_model(&connection, &actual);
            let desired = canonicalize_model(&connection, desired);
            let applied_ids = connection
                .applied_refactor_ids()
                .await
                .map_err(|error| CliError::Message(format!("read applied refactors: {error}")))?;
            let metadata = connection
                .schema_metadata()
                .await
                .map_err(|error| CliError::Message(format!("read schema metadata: {error}")))?;
            let publish_history = connection
                .schema_publish_history(history)
                .await
                .map_err(|error| CliError::Message(format!("read publish history: {error}")))?;
            Ok((desired, actual, applied_ids, metadata, publish_history))
        }
        BackendKind::Mysql => {
            let mut connection = Mysql.connect(url).await.map_err(|error| {
                CliError::Message(format!(
                    "connect: {}",
                    redact_secret(&error.to_string(), url)
                ))
            })?;
            let actual = introspect(&mut connection)
                .await
                .map_err(|error| CliError::Message(format!("introspect: {error}")))?;
            // Canonicalize both sides so equivalent predicate / CHECK expressions compare equal.
            let actual = canonicalize_model(&connection, &actual);
            let desired = canonicalize_model(&connection, desired);
            let applied_ids = connection
                .applied_refactor_ids()
                .await
                .map_err(|error| CliError::Message(format!("read applied refactors: {error}")))?;
            let metadata = connection
                .schema_metadata()
                .await
                .map_err(|error| CliError::Message(format!("read schema metadata: {error}")))?;
            let publish_history = connection
                .schema_publish_history(history)
                .await
                .map_err(|error| CliError::Message(format!("read publish history: {error}")))?;
            Ok((desired, actual, applied_ids, metadata, publish_history))
        }
    }
}

async fn record_publish_metadata<C>(
    loaded: &LoadedModel,
    mode: &str,
    connection: &mut C,
) -> Result<(), CliError>
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
        .map_err(|error| CliError::Message(format!("record schema metadata: {error}")))?;
    connection
        .record_schema_publish(mode, package_hash, package_format_version)
        .await
        .map_err(|error| CliError::Message(format!("record publish history: {error}")))
}

fn metadata_value<'metadata>(
    metadata: &'metadata [(String, String)],
    key: &str,
) -> Result<&'metadata str, CliError> {
    metadata
        .iter()
        .find(|(metadata_key, _)| metadata_key == key)
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| CliError::Message(format!("missing package metadata key `{key}`")))
}

async fn run_refactors(command: RefactorsCommand) -> Result<(), CliError> {
    match command {
        RefactorsCommand::List {
            backend,
            url,
            package,
        } => {
            let applied_ids = applied_refactor_ids(backend.backend, &url).await?;
            if let Some(package) = package {
                let refactors = read_refactor_log(&package).map_err(|error| {
                    CliError::Message(format!("read package refactors: {error}"))
                })?;
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
            validate_url(backend.backend, &url)?;
            let refactors = read_refactor_log(&package)
                .map_err(|error| CliError::Message(format!("read package refactors: {error}")))?;
            match backend.backend {
                BackendKind::Postgres => {
                    let mut connection = Postgres.connect(&url).await.map_err(|error| {
                        CliError::Message(format!(
                            "connect: {}",
                            redact_secret(&error.to_string(), url)
                        ))
                    })?;
                    let report = repair_refactor_metadata(&refactors, &mut connection)
                        .await
                        .map_err(|error| {
                            CliError::Message(format!("repair refactor metadata: {error}"))
                        })?;
                    print_refactor_repair_report(&report);
                    Ok(())
                }
                BackendKind::Mysql => {
                    let mut connection = Mysql.connect(&url).await.map_err(|error| {
                        CliError::Message(format!(
                            "connect: {}",
                            redact_secret(&error.to_string(), url)
                        ))
                    })?;
                    let report = repair_refactor_metadata(&refactors, &mut connection)
                        .await
                        .map_err(|error| {
                            CliError::Message(format!("repair refactor metadata: {error}"))
                        })?;
                    print_refactor_repair_report(&report);
                    Ok(())
                }
            }
        }
    }
}

async fn applied_refactor_ids(backend: BackendKind, url: &str) -> Result<Vec<String>, CliError> {
    validate_url(backend, url)?;
    match backend {
        BackendKind::Postgres => {
            let mut connection = Postgres.connect(url).await.map_err(|error| {
                CliError::Message(format!(
                    "connect: {}",
                    redact_secret(&error.to_string(), url)
                ))
            })?;
            connection
                .applied_refactor_ids()
                .await
                .map_err(|error| CliError::Message(format!("read applied refactors: {error}")))
        }
        BackendKind::Mysql => {
            let mut connection = Mysql.connect(url).await.map_err(|error| {
                CliError::Message(format!(
                    "connect: {}",
                    redact_secret(&error.to_string(), url)
                ))
            })?;
            connection
                .applied_refactor_ids()
                .await
                .map_err(|error| CliError::Message(format!("read applied refactors: {error}")))
        }
    }
}

/// Masks the password from a connection URL wherever it appears in `message`, so credentials
/// never leak into error output or logs even if a driver echoes the DSN it was given.
fn redact_secret(message: &str, url: impl AsRef<str>) -> String {
    match url_password(url.as_ref()) {
        Some(password) if !password.is_empty() => message.replace(password, "***"),
        _ => message.to_owned(),
    }
}

/// Extracts the password component of a `scheme://user:password@host/...` connection URL.
fn url_password(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let userinfo = after_scheme[..authority_end].rsplit_once('@')?.0;
    Some(userinfo.split_once(':')?.1)
}

/// Validates a connection URL before we hand it to a driver, so a malformed URL or a
/// backend/scheme mismatch fails with a clear, credential-free message instead of a cryptic
/// driver error. Any URL echoed in an error is password-redacted.
fn validate_url(backend: BackendKind, url: &str) -> Result<(), CliError> {
    let Some((scheme, after)) = url.split_once("://") else {
        return Err(CliError::Message(format!(
            "invalid connection URL `{}`: expected `scheme://...`",
            redact_secret(url, url)
        )));
    };
    let scheme = scheme.to_ascii_lowercase();
    let scheme_ok = match backend {
        BackendKind::Postgres => matches!(scheme.as_str(), "postgres" | "postgresql"),
        BackendKind::Mysql => scheme == "mysql",
    };
    if !scheme_ok {
        return Err(CliError::Message(format!(
            "connection URL scheme `{scheme}` does not match backend `{}`",
            backend.value_name()
        )));
    }
    let authority = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_userinfo, host)| host);
    if host.is_empty() {
        return Err(CliError::Message(format!(
            "invalid connection URL `{}`: missing host",
            redact_secret(url, url)
        )));
    }
    Ok(())
}

/// Sets session-level lock/statement timeouts before a publish so a migration cannot block
/// indefinitely behind a held lock. The `SET`s are session-scoped and persist for the connection.
async fn apply_session_timeouts<C>(
    connection: &mut C,
    backend: BackendKind,
    lock_timeout: Option<u64>,
    statement_timeout: Option<u64>,
) -> Result<(), CliError>
where
    C: DdlExecutor,
    C::Error: std::fmt::Display,
{
    if matches!(backend, BackendKind::Mysql) && statement_timeout.is_some() {
        tracing::warn!("--statement-timeout is ignored for MySQL (no DDL statement timeout)");
    }
    let statements = session_timeout_statements(backend, lock_timeout, statement_timeout);
    if statements.is_empty() {
        return Ok(());
    }
    connection
        .execute_ddl(&statements.join(";\n"))
        .await
        .map_err(|error| CliError::Message(format!("set session timeouts: {error}")))
}

/// Requires explicit confirmation before applying a plan that contains destructive steps. With
/// `assume_yes` the confirmation is taken as given (for automation); otherwise an interactive
/// terminal is prompted, and a non-interactive stdin is refused so destructive changes are never
/// applied unattended.
fn confirm_destructive(plan: &DatabasePlan, assume_yes: bool) -> Result<(), CliError> {
    use std::io::{IsTerminal, Write};

    let destructive: Vec<_> = classified_plan_steps(plan)
        .into_iter()
        .filter(|classified| matches!(classified.risk, ChangeRisk::Destructive))
        .collect();
    if destructive.is_empty() {
        return Ok(());
    }

    eprintln!(
        "This publish includes {} destructive change(s):",
        destructive.len()
    );
    for classified in &destructive {
        eprintln!("  - {}", describe_plan_step(&classified.step));
    }

    if assume_yes {
        eprintln!("Proceeding because --yes was given.");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(CliError::Message(
            "destructive changes require confirmation; re-run with --yes to apply".to_owned(),
        ));
    }
    eprint!("Type 'yes' to apply these changes: ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| CliError::Message(format!("read confirmation: {error}")))?;
    if answer.trim() == "yes" {
        Ok(())
    } else {
        Err(CliError::Message(
            "aborted: destructive changes were not confirmed".to_owned(),
        ))
    }
}

/// A short human description of a plan step, used in the destructive-change confirmation prompt.
fn describe_plan_step(step: &DatabasePlanStep) -> String {
    match step {
        DatabasePlanStep::CreateSchema { schema } => {
            format!("create schema {}", schema_name(schema))
        }
        DatabasePlanStep::DropSchema { schema } => format!("drop schema {}", schema_name(schema)),
        DatabasePlanStep::CreateTable { schema, table } => {
            format!("create table {}", qualified(schema, &table.name))
        }
        DatabasePlanStep::DropTable { schema, table } => {
            format!("drop table {}", qualified(schema, &table.name))
        }
        DatabasePlanStep::RenameTable {
            schema, from, to, ..
        } => format!("rename table {} to {to}", qualified(schema, from)),
        DatabasePlanStep::AlterTable {
            schema,
            table,
            change,
        } => format!(
            "{} on {}",
            describe_table_plan_step(change),
            qualified(schema, table)
        ),
        DatabasePlanStep::CreateView { schema, view } => {
            format!("create view {}", qualified(schema, &view.name))
        }
        DatabasePlanStep::DropView { schema, view } => {
            format!("drop view {}", qualified(schema, &view.name))
        }
    }
}

fn describe_table_plan_step(step: &TablePlanStep) -> String {
    match step {
        TablePlanStep::SetTableComment { .. } => "set comment".to_owned(),
        TablePlanStep::AddColumn { column } => format!("add column {}", column.name),
        TablePlanStep::DropColumn { column } => format!("drop column {}", column.name),
        TablePlanStep::RenameColumn { from, to, .. } => format!("rename column {from} to {to}"),
        TablePlanStep::AlterColumn { after, .. } => format!("alter column {}", after.name),
        TablePlanStep::AddPrimaryKey { .. } => "add primary key".to_owned(),
        TablePlanStep::DropPrimaryKey { .. } => "drop primary key".to_owned(),
        TablePlanStep::AlterPrimaryKey { .. } => "alter primary key".to_owned(),
        TablePlanStep::AddUnique { constraint } => format!("add unique {}", constraint.name),
        TablePlanStep::DropUnique { constraint } => format!("drop unique {}", constraint.name),
        TablePlanStep::AlterUnique { after, .. } => format!("alter unique {}", after.name),
        TablePlanStep::AddForeignKey { foreign_key } => {
            format!("add foreign key {}", foreign_key.name)
        }
        TablePlanStep::DropForeignKey { foreign_key } => {
            format!("drop foreign key {}", foreign_key.name)
        }
        TablePlanStep::AlterForeignKey { after, .. } => format!("alter foreign key {}", after.name),
        TablePlanStep::AddCheck { check } => format!("add check {}", check.name),
        TablePlanStep::DropCheck { check } => format!("drop check {}", check.name),
        TablePlanStep::AlterCheck { after, .. } => format!("alter check {}", after.name),
        TablePlanStep::AddIndex { index } => format!("add index {}", index.name),
        TablePlanStep::DropIndex { index } => format!("drop index {}", index.name),
        TablePlanStep::AlterIndex { after, .. } => format!("alter index {}", after.name),
    }
}

/// Builds the backend-specific `SET` statements for the requested session timeouts (empty when none
/// are requested). MySQL has no DDL statement timeout, so `statement_timeout` is dropped there.
fn session_timeout_statements(
    backend: BackendKind,
    lock_timeout: Option<u64>,
    statement_timeout: Option<u64>,
) -> Vec<String> {
    let mut statements = Vec::new();
    match backend {
        BackendKind::Postgres => {
            if let Some(secs) = lock_timeout {
                statements.push(format!("SET lock_timeout = '{secs}s'"));
            }
            if let Some(secs) = statement_timeout {
                statements.push(format!("SET statement_timeout = '{secs}s'"));
            }
        }
        BackendKind::Mysql => {
            if let Some(secs) = lock_timeout {
                statements.push(format!("SET SESSION lock_wait_timeout = {secs}"));
            }
        }
    }
    statements
}

/// Loads a model for `diff`/`plan` from either a crate database type or a `.sqz` package. `side` is
/// the argument prefix (`desired`/`actual`) used in error and usage messages.
fn load_model(
    database: Option<String>,
    package: Option<PathBuf>,
    side: &str,
) -> Result<DatabaseModel, CliError> {
    match (database, package) {
        (Some(database), None) => extract_model(&database),
        (None, Some(package)) => read_package(&package)
            .map_err(|error| CliError::Message(format!("read {side}: {error}"))),
        _ => Err(CliError::Message(format!(
            "provide exactly one of --{side}-database or --{side}"
        ))),
    }
}

/// Loads the desired model plus its refactor log for `plan`: from a `.sqz` package (which carries an
/// embedded refactor log) or a crate database type (whose refactors come from an optional `--refactors`
/// file).
fn load_desired_model(
    database: Option<String>,
    package: Option<PathBuf>,
    refactors: Option<PathBuf>,
) -> Result<LoadedModel, CliError> {
    match (database, package) {
        (Some(database), None) => {
            let model = extract_model(&database)?;
            let refactors = match refactors {
                Some(path) => read_refactor_file(&path)?,
                None => RefactorLog::default(),
            };
            Ok(LoadedModel { model, refactors })
        }
        (None, Some(package)) => {
            let model = read_package(&package)
                .map_err(|error| CliError::Message(format!("read desired: {error}")))?;
            let refactors = read_refactor_log(&package)
                .map_err(|error| CliError::Message(format!("read desired refactors: {error}")))?;
            Ok(LoadedModel { model, refactors })
        }
        _ => Err(CliError::Message(
            "provide exactly one of --desired-database or --desired".to_owned(),
        )),
    }
}

fn read_refactor_file(path: &Path) -> Result<RefactorLog, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| CliError::Message(format!("read refactor file: {error}")))?;
    refactor_from_kdl(&text)
        .map_err(|error| CliError::Message(format!("parse refactor file: {error}")))
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
    let summary = refactor_status_summary(refactors, applied_ids);

    for id in &summary.applied {
        println!("applied {id}");
    }

    for id in &summary.pending {
        println!("pending {id}");
    }

    for id in &summary.recorded_only {
        println!("recorded-only {id}");
    }

    if summary.applied.is_empty() && summary.pending.is_empty() && summary.recorded_only.is_empty()
    {
        println!("no refactors");
    }
}

struct RefactorStatusSummary {
    applied: Vec<String>,
    pending: Vec<String>,
    recorded_only: Vec<String>,
}

fn refactor_status_summary(
    refactors: &RefactorLog,
    applied_ids: &[String],
) -> RefactorStatusSummary {
    let applied_ids = applied_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    // Casts (`cast-column`) are idempotent rendering hints that are deliberately never recorded as
    // applied refactors (`RefactorOperation::is_recorded` is false, and `repair_refactor_metadata`
    // filters to recorded ops). Including them here would report every cast as pending forever, so a
    // clean `status --check-refactors` would be impossible after a publish that used one.
    let package_ids = refactors
        .operations
        .iter()
        .filter(|operation| operation.is_recorded())
        .map(|operation| operation.id())
        .collect::<BTreeSet<_>>();
    let mut applied = Vec::new();
    let mut pending = Vec::new();

    for operation in refactors
        .operations
        .iter()
        .filter(|operation| operation.is_recorded())
    {
        let id = operation.id();
        if applied_ids.contains(id) {
            applied.push(id.to_owned());
        } else {
            pending.push(id.to_owned());
        }
    }

    let recorded_only = applied_ids
        .difference(&package_ids)
        .map(|id| (*id).to_owned())
        .collect();

    RefactorStatusSummary {
        applied,
        pending,
        recorded_only,
    }
}

struct MetadataStatusEntry<'a> {
    key: &'a str,
    status: &'static str,
    desired: &'a str,
    actual: Option<&'a str>,
}

fn metadata_status_entries<'a>(
    desired_metadata: &'a [(String, String)],
    live_metadata: &'a [(String, String)],
) -> Vec<MetadataStatusEntry<'a>> {
    let live = live_metadata
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<BTreeSet<_>>();
    let live_values = live_metadata
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<BTreeMap<_, _>>();

    desired_metadata
        .iter()
        .map(|(key, desired_value)| {
            let actual = live_values.get(key.as_str()).copied();
            let status = if live.contains(&(key.as_str(), desired_value.as_str())) {
                "match"
            } else if actual.is_some() {
                "mismatch"
            } else {
                "missing"
            };

            MetadataStatusEntry {
                key,
                status,
                desired: desired_value,
                actual,
            }
        })
        .collect()
}

fn print_status_json(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
    refactors: &RefactorLog,
    applied_ids: &[String],
    desired_metadata: &[(String, String)],
    live_metadata: &[(String, String)],
    publish_history: &[SchemaPublishRecord],
) -> Result<(), CliError> {
    let diff = diff_models(desired, actual);
    let refactors = refactor_status_summary(refactors, applied_ids);
    let metadata = metadata_status_entries(desired_metadata, live_metadata)
        .into_iter()
        .map(|entry| {
            json!({
                "key": entry.key,
                "status": entry.status,
                "desired": entry.desired,
                "actual": entry.actual,
            })
        })
        .collect::<Vec<_>>();
    let publish_history = publish_history
        .iter()
        .enumerate()
        .map(|(index, record)| {
            let index = index + 1;
            json!({
                "index": index,
                "label": if index == 1 { "latest" } else { "entry" },
                "mode": record.mode,
                "package_hash": record.package_hash,
                "package_format_version": record.package_format_version,
                "applied_at": record.applied_at,
            })
        })
        .collect::<Vec<_>>();

    let status = json!({
        "schema": {
            "clean": diff.is_empty(),
            "change_count": diff.changes.len(),
            "changes": diff_changes_json(&diff),
        },
        "refactors": {
            "applied": refactors.applied,
            "pending": refactors.pending,
            "recorded_only": refactors.recorded_only,
        },
        "metadata": metadata,
        "publish_history": publish_history,
    });

    let json = serde_json::to_string_pretty(&status)
        .map_err(|error| CliError::Message(format!("render status json: {error}")))?;
    println!("{json}");
    Ok(())
}

fn diff_changes_json(diff: &squealy_model::DatabaseDiff) -> Vec<serde_json::Value> {
    diff.changes.iter().map(database_diff_change_json).collect()
}

fn database_diff_change_json(change: &DatabaseDiffChange) -> serde_json::Value {
    let risk = risk_name(change.risk());
    match change {
        DatabaseDiffChange::CreateSchema { schema } => {
            json!({
                "kind": "schema",
                "action": "create",
                "risk": risk,
                "schema": schema,
                "name": schema_name(schema),
            })
        }
        DatabaseDiffChange::DropSchema { schema } => {
            json!({
                "kind": "schema",
                "action": "drop",
                "risk": risk,
                "schema": schema,
                "name": schema_name(schema),
            })
        }
        DatabaseDiffChange::CreateTable { schema, table } => {
            json!({
                "kind": "table",
                "action": "create",
                "risk": risk,
                "schema": schema,
                "table": table.name,
                "name": qualified(schema, &table.name),
            })
        }
        DatabaseDiffChange::DropTable { schema, table } => {
            json!({
                "kind": "table",
                "action": "drop",
                "risk": risk,
                "schema": schema,
                "table": table.name,
                "name": qualified(schema, &table.name),
            })
        }
        DatabaseDiffChange::AlterTable {
            schema,
            table,
            changes,
        } => {
            let changes = changes
                .iter()
                .map(|change| table_diff_change_json(schema, table, change))
                .collect::<Vec<_>>();
            json!({
                "kind": "table",
                "action": "alter",
                "risk": risk,
                "schema": schema,
                "table": table,
                "name": qualified(schema, table),
                "changes": changes,
            })
        }
        DatabaseDiffChange::CreateView { schema, view } => {
            json!({
                "kind": "view",
                "action": "create",
                "risk": risk,
                "schema": schema,
                "view": view.name,
                "name": qualified(schema, &view.name),
            })
        }
        DatabaseDiffChange::DropView { schema, view } => {
            json!({
                "kind": "view",
                "action": "drop",
                "risk": risk,
                "schema": schema,
                "view": view.name,
                "name": qualified(schema, &view.name),
            })
        }
    }
}

fn table_diff_change_json(
    schema: &Option<String>,
    table: &str,
    change: &TableDiffChange,
) -> serde_json::Value {
    let risk = risk_name(change.risk());
    let table_name = qualified(schema, table);
    match change {
        TableDiffChange::SetTableComment { .. } => {
            json!({
                "kind": "table_comment",
                "action": "set",
                "risk": risk,
                "schema": schema,
                "table": table,
                "name": table_name,
            })
        }
        TableDiffChange::AddColumn { column } => {
            json!({
                "kind": "column",
                "action": "add",
                "risk": risk,
                "schema": schema,
                "table": table,
                "column": column.name,
                "name": format!("{table_name}.{}", column.name),
            })
        }
        TableDiffChange::DropColumn { column } => {
            json!({
                "kind": "column",
                "action": "drop",
                "risk": risk,
                "schema": schema,
                "table": table,
                "column": column.name,
                "name": format!("{table_name}.{}", column.name),
            })
        }
        TableDiffChange::AlterColumn { after, .. } => {
            json!({
                "kind": "column",
                "action": "alter",
                "risk": risk,
                "schema": schema,
                "table": table,
                "column": after.name,
                "name": format!("{table_name}.{}", after.name),
            })
        }
        TableDiffChange::AddPrimaryKey { constraint } => {
            constraint_change_json("primary_key", "add", risk, schema, table, &constraint.name)
        }
        TableDiffChange::DropPrimaryKey { constraint } => {
            constraint_change_json("primary_key", "drop", risk, schema, table, &constraint.name)
        }
        TableDiffChange::AlterPrimaryKey { after, .. } => {
            constraint_change_json("primary_key", "alter", risk, schema, table, &after.name)
        }
        TableDiffChange::AddUnique { constraint } => {
            constraint_change_json("unique", "add", risk, schema, table, &constraint.name)
        }
        TableDiffChange::DropUnique { constraint } => {
            constraint_change_json("unique", "drop", risk, schema, table, &constraint.name)
        }
        TableDiffChange::AlterUnique { after, .. } => {
            constraint_change_json("unique", "alter", risk, schema, table, &after.name)
        }
        TableDiffChange::AddForeignKey { foreign_key } => {
            constraint_change_json("foreign_key", "add", risk, schema, table, &foreign_key.name)
        }
        TableDiffChange::DropForeignKey { foreign_key } => constraint_change_json(
            "foreign_key",
            "drop",
            risk,
            schema,
            table,
            &foreign_key.name,
        ),
        TableDiffChange::AlterForeignKey { after, .. } => {
            constraint_change_json("foreign_key", "alter", risk, schema, table, &after.name)
        }
        TableDiffChange::AddCheck { check } => {
            constraint_change_json("check", "add", risk, schema, table, &check.name)
        }
        TableDiffChange::DropCheck { check } => {
            constraint_change_json("check", "drop", risk, schema, table, &check.name)
        }
        TableDiffChange::AlterCheck { after, .. } => {
            constraint_change_json("check", "alter", risk, schema, table, &after.name)
        }
        TableDiffChange::AddIndex { index } => {
            constraint_change_json("index", "add", risk, schema, table, &index.name)
        }
        TableDiffChange::DropIndex { index } => {
            constraint_change_json("index", "drop", risk, schema, table, &index.name)
        }
        TableDiffChange::AlterIndex { after, .. } => {
            constraint_change_json("index", "alter", risk, schema, table, &after.name)
        }
    }
}

fn constraint_change_json(
    kind: &'static str,
    action: &'static str,
    risk: &'static str,
    schema: &Option<String>,
    table: &str,
    object_name: &str,
) -> serde_json::Value {
    let table_name = qualified(schema, table);
    json!({
        "kind": kind,
        "action": action,
        "risk": risk,
        "schema": schema,
        "table": table,
        "object": object_name,
        "name": format!("{table_name}.{object_name}"),
    })
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

fn check_status(
    checks: &StatusChecks,
    desired: &DatabaseModel,
    actual: &DatabaseModel,
    refactors: &RefactorLog,
    applied_ids: &[String],
    desired_metadata: &[(String, String)],
    live_metadata: &[(String, String)],
) -> Result<(), CliError> {
    let mut failures = Vec::new();

    if checks.schema && !diff_models(desired, actual).is_empty() {
        failures.push("schema");
    }
    if checks.refactors && refactors_need_attention(refactors, applied_ids) {
        failures.push("refactors");
    }
    if checks.metadata && metadata_needs_attention(desired_metadata, live_metadata) {
        failures.push("metadata");
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(CliError::Message(format!(
            "status check failed: {}",
            failures.join(", ")
        )))
    }
}

fn refactors_need_attention(refactors: &RefactorLog, applied_ids: &[String]) -> bool {
    let summary = refactor_status_summary(refactors, applied_ids);
    !summary.pending.is_empty() || !summary.recorded_only.is_empty()
}

fn metadata_needs_attention(
    desired_metadata: &[(String, String)],
    live_metadata: &[(String, String)],
) -> bool {
    metadata_status_entries(desired_metadata, live_metadata)
        .iter()
        .any(|entry| entry.status != "match")
}

fn print_metadata_status(
    desired_metadata: &[(String, String)],
    live_metadata: &[(String, String)],
) {
    for entry in metadata_status_entries(desired_metadata, live_metadata) {
        println!("metadata {} {}", entry.key, entry.status);
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

    for (index, record) in publish_history.iter().enumerate().skip(1) {
        println!(
            "publish-history entry index={} mode={} package.content_hash={} package.format_version={} applied_at={}",
            index + 1,
            record.mode,
            record.package_hash,
            record.package_format_version,
            record.applied_at
        );
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
            DatabaseDiffChange::CreateView { schema, view } => {
                println!("{risk} view + {}", qualified(schema, &view.name));
            }
            DatabaseDiffChange::DropView { schema, view } => {
                println!("{risk} view - {}", qualified(schema, &view.name));
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

/// Builds the error shown when a plan is refused because it contains destructive or ambiguous
/// changes. Lists each blocked change with its risk and points at the flags that force them.
fn policy_blocked_error(blocked: &[ClassifiedDatabaseDiffChange]) -> CliError {
    let mut message = format!(
        "refusing to apply {} change(s) blocked by policy:",
        blocked.len()
    );
    for classified in blocked {
        message.push_str(&format!(
            "\n  {} {}",
            risk_name(classified.risk),
            describe_diff_change(&classified.change)
        ));
    }
    message.push_str(
        "\nre-run with --allow-destructive and/or --allow-ambiguous to force these changes.",
    );
    CliError::Message(message)
}

/// A one-line description of a top-level diff change, for the policy-block message.
fn describe_diff_change(change: &DatabaseDiffChange) -> String {
    match change {
        DatabaseDiffChange::CreateSchema { schema } => {
            format!("create schema {}", schema_name(schema))
        }
        DatabaseDiffChange::DropSchema { schema } => format!("drop schema {}", schema_name(schema)),
        DatabaseDiffChange::CreateTable { schema, table } => {
            format!("create table {}", qualified(schema, &table.name))
        }
        DatabaseDiffChange::DropTable { schema, table } => {
            format!("drop table {}", qualified(schema, &table.name))
        }
        DatabaseDiffChange::AlterTable { schema, table, .. } => {
            format!("alter table {}", qualified(schema, table))
        }
        DatabaseDiffChange::CreateView { schema, view } => {
            format!("create view {}", qualified(schema, &view.name))
        }
        DatabaseDiffChange::DropView { schema, view } => {
            format!("drop view {}", qualified(schema, &view.name))
        }
    }
}

/// Resolves a plan-from-database result, turning a policy block into the actionable
/// [`policy_blocked_error`] and any other failure into a contextual message.
fn plan_from_database_result<E: std::fmt::Display>(
    result: Result<DatabasePlan, PlanFromDatabaseError<E>>,
) -> Result<DatabasePlan, CliError> {
    match result {
        Ok(plan) => Ok(plan),
        Err(PlanFromDatabaseError::Policy(error)) => Err(policy_blocked_error(&error.blocked)),
        Err(other) => Err(CliError::Message(format!("plan: {other}"))),
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

#[cfg(test)]
mod tests {
    use super::{
        BackendKind, ChangeRisk, ClassifiedDatabaseDiffChange, DatabaseDiffChange, DatabasePlan,
        DatabasePlanStep, policy_blocked_error, redact_secret, url_password, validate_url,
    };

    #[test]
    fn refactor_status_summary_excludes_cast_hints() {
        use super::refactor_status_summary;
        use squealy_model::{CastColumn, RefactorLog, RefactorOperation, RenameColumn};

        let refactors = RefactorLog {
            operations: vec![
                RefactorOperation::RenameColumn(RenameColumn {
                    id: "rename-1".to_owned(),
                    schema: None,
                    table: "users".to_owned(),
                    from: "email".to_owned(),
                    to: "email_address".to_owned(),
                }),
                RefactorOperation::CastColumn(CastColumn {
                    id: "cast-1".to_owned(),
                    schema: None,
                    table: "orders".to_owned(),
                    column: "total".to_owned(),
                    using: "total::numeric".to_owned(),
                }),
            ],
        };

        // The rename was applied; the cast (never recorded as applied) must not surface as pending,
        // so `status --check-refactors` can be clean after a publish that used the cast.
        let summary = refactor_status_summary(&refactors, &["rename-1".to_owned()]);
        assert_eq!(summary.applied, vec!["rename-1".to_owned()]);
        assert!(summary.pending.is_empty(), "{:?}", summary.pending);
        assert!(
            summary.recorded_only.is_empty(),
            "{:?}",
            summary.recorded_only
        );
    }

    #[test]
    fn policy_blocked_error_lists_changes_and_force_flags() {
        let blocked = vec![ClassifiedDatabaseDiffChange {
            risk: ChangeRisk::Destructive,
            change: DatabaseDiffChange::DropSchema {
                schema: Some("old".to_owned()),
            },
        }];
        let error = policy_blocked_error(&blocked).to_string();
        assert!(error.contains("destructive drop schema old"), "{error}");
        assert!(error.contains("--allow-destructive"), "{error}");
        assert!(error.contains("--allow-ambiguous"), "{error}");
    }

    #[test]
    fn validate_url_accepts_matching_scheme() {
        assert!(validate_url(BackendKind::Postgres, "postgres://u:p@host:5432/db").is_ok());
        assert!(validate_url(BackendKind::Postgres, "postgresql://host/db").is_ok());
        assert!(validate_url(BackendKind::Mysql, "mysql://root@127.0.0.1:3306/db").is_ok());
    }

    #[test]
    fn validate_url_rejects_backend_scheme_mismatch() {
        let error = validate_url(BackendKind::Mysql, "postgres://host/db")
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match backend `mysql`"));
    }

    #[test]
    fn validate_url_rejects_malformed_and_redacts() {
        assert!(validate_url(BackendKind::Postgres, "host/db").is_err());
        let error = validate_url(BackendKind::Postgres, "postgres://u:s3cret@/db")
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing host"));
        assert!(!error.contains("s3cret"));
    }

    #[test]
    fn confirm_destructive_allows_non_destructive_plan() {
        use super::confirm_destructive;
        assert!(confirm_destructive(&DatabasePlan::default(), false).is_ok());
    }

    #[test]
    fn confirm_destructive_proceeds_with_assume_yes() {
        use super::confirm_destructive;
        let plan = DatabasePlan {
            steps: vec![DatabasePlanStep::DropSchema {
                schema: Some("old".to_owned()),
            }],
        };
        assert!(confirm_destructive(&plan, true).is_ok());
    }

    #[test]
    fn confirm_destructive_refuses_without_tty_or_yes() {
        use super::confirm_destructive;
        let plan = DatabasePlan {
            steps: vec![DatabasePlanStep::DropSchema {
                schema: Some("old".to_owned()),
            }],
        };
        // Test stdin is not a terminal, so confirmation must be refused without --yes.
        let error = confirm_destructive(&plan, false).unwrap_err().to_string();
        assert!(error.contains("--yes"));
    }

    #[test]
    fn redact_credentials_masks_passwords_in_log_lines() {
        use super::redact_credentials;
        let redacted =
            redact_credentials("connecting to postgres://user:s3cret@db:5432/app timed out");
        assert!(!redacted.contains("s3cret"));
        assert!(redacted.contains("postgres://user:***@db:5432/app"));
        // A password containing `@` must still be fully masked (split on the last `@`).
        let at = redact_credentials("postgres://user:p@ss@db/app");
        assert!(!at.contains("p@ss") && !at.contains("ss@db"), "{at}");
        assert_eq!(at, "postgres://user:***@db/app");
        // No userinfo password: left untouched.
        assert_eq!(
            redact_credentials("mysql://root@host/db"),
            "mysql://root@host/db"
        );
        // Text without a URL is unchanged.
        assert_eq!(redact_credentials("schema clean"), "schema clean");
    }

    #[test]
    fn session_timeouts_render_per_backend() {
        use super::session_timeout_statements;
        assert_eq!(
            session_timeout_statements(BackendKind::Postgres, Some(5), Some(30)),
            vec![
                "SET lock_timeout = '5s'".to_owned(),
                "SET statement_timeout = '30s'".to_owned(),
            ]
        );
        // MySQL has no DDL statement timeout, so only the lock timeout is emitted.
        assert_eq!(
            session_timeout_statements(BackendKind::Mysql, Some(5), Some(30)),
            vec!["SET SESSION lock_wait_timeout = 5".to_owned()]
        );
        assert!(session_timeout_statements(BackendKind::Postgres, None, None).is_empty());
    }

    #[test]
    fn url_password_extracts_password_component() {
        assert_eq!(
            url_password("postgres://user:s3cret@localhost:5432/db"),
            Some("s3cret")
        );
        assert_eq!(url_password("mysql://root@127.0.0.1:3306/db"), None);
        assert_eq!(url_password("postgres://localhost/db"), None);
        assert_eq!(url_password("not-a-url"), None);
    }

    #[test]
    fn redact_secret_masks_password_in_messages() {
        let url = "postgres://user:s3cret@localhost:5432/db";
        let message = "error connecting with postgres://user:s3cret@localhost:5432/db: timed out";
        let redacted = redact_secret(message, url);
        assert!(!redacted.contains("s3cret"));
        assert!(redacted.contains("***"));
    }

    #[test]
    fn redact_secret_is_noop_without_password() {
        let message = "could not resolve host";
        assert_eq!(redact_secret(message, "mysql://root@127.0.0.1/db"), message);
    }
}
