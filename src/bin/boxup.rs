use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use boxup::backend::Backend;
use boxup::config::BrowseConfig;
use boxup::domain::{
    BackupPhase, BackupProgress, BackupProtocolEvent, BackupProtocolEventKind, Snapshot, utc_now,
};
use boxup::index::{Index, IndexStatus};
use boxup::jobs::{DockerManager, JobRunner, LocalLock, LockMode};
use boxup::restore::{restore, validate_mountpoint};
use boxup::{BorgBackend, Config};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use futures::TryStreamExt;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "boxup", version, about = "Safe Borg 1.4 backup management")]
struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        default_value = "~/.config/boxup/config.toml"
    )]
    config: PathBuf,
    #[arg(long, global = true, value_name = "FILE")]
    browse_config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Setup,
    #[command(hide = true)]
    Notify {
        #[arg(long)]
        watch: bool,
        #[arg(long, hide = true, value_name = "PATH")]
        state_file: Option<PathBuf>,
    },
    Automation {
        #[command(subcommand)]
        command: AutomationCommand,
    },
    Init,
    Backup {
        #[command(subcommand)]
        command: Option<BackupCommand>,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Snapshots {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        live: bool,
    },
    Ls {
        snapshot: String,
        path: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        live: bool,
    },
    Search {
        query: String,
        #[arg(long)]
        all_snapshots: bool,
        #[arg(long)]
        json: bool,
    },
    Restore(RestoreArgs),
    Mount {
        snapshot: String,
        target: PathBuf,
    },
    Umount {
        target: PathBuf,
    },
    Diff {
        a: String,
        b: String,
        path: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Prune {
        #[arg(long)]
        dry_run: bool,
    },
    Check {
        #[arg(long)]
        verify_data: bool,
    },
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    Tui,
}

#[derive(Subcommand)]
enum BackupCommand {
    Run,
}

#[derive(Subcommand, Clone, Copy)]
enum AutomationCommand {
    Enable,
    Disable,
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum IndexCommand {
    Refresh,
}

#[derive(Subcommand)]
enum KeyCommand {
    Export {
        #[arg(long, value_name = "PATH")]
        to: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    Validate {
        #[arg(long, value_name = "PATH")]
        system_profile: Option<PathBuf>,
    },
    BrowseDescriptor {
        #[arg(long, value_name = "PATH")]
        system_profile: PathBuf,
    },
    #[command(hide = true)]
    CredentialRequirements,
}

#[derive(Subcommand)]
enum AuditCommand {
    Docker {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        running: bool,
    },
}

#[derive(Args)]
struct RestoreArgs {
    snapshot: String,
    #[arg(required = true)]
    paths: Vec<String>,
    #[arg(long, value_name = "PATH")]
    to: PathBuf,
    #[arg(long)]
    overwrite: bool,
    #[arg(long)]
    sudo: bool,
}

#[derive(Serialize)]
struct StatusOutput {
    host: String,
    repository: String,
    last_backup: Option<chrono::DateTime<Utc>>,
    last_backup_details: Option<boxup::domain::JobRecord>,
    due: bool,
    next_due: Option<chrono::DateTime<Utc>>,
    index: IndexStatus,
    index_usable: bool,
    operation_active: bool,
    estimated_backup_seconds: Option<u64>,
    latest_backup_attempt: Option<boxup::domain::JobRecord>,
    running_jobs: Vec<boxup::domain::JobRecord>,
    jobs: Vec<boxup::domain::JobRecord>,
}

const INDEX_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = run().await {
        tracing::error!("{error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut cli = Cli::parse();
    let command = cli.command.take().unwrap_or(Commands::Tui);
    let default_config_selected = cli.config == Path::new("~/.config/boxup/config.toml");
    expand_config_path(&mut cli.config)?;
    if matches!(command, Commands::Setup) {
        return boxup::setup::run_wizard().await;
    }
    if cli.browse_config.is_none()
        && !nix::unistd::Uid::effective().is_root()
        && default_config_selected
        && !cli.config.exists()
        && supports_auto_browse(&command)
    {
        cli.browse_config = discover_browse_config()?;
    }
    if let Some(path) = &mut cli.browse_config {
        expand_config_path(path)?;
        let browse = BrowseConfig::load(path)?;
        return run_browse(command, browse).await;
    }
    if default_config_selected && !cli.config.exists() && matches!(command, Commands::Tui) {
        return boxup::setup::run_wizard().await;
    }
    if let Commands::Restore(args) = &command {
        if args.overwrite {
            ensure!(args.sudo, "--overwrite requires --sudo");
            ensure!(
                args.to == Path::new("/"),
                "--overwrite is allowed only with --to /"
            );
            ensure!(
                std::io::stdin().is_terminal(),
                "overwrite restore requires a TTY"
            );
            invoke_root_restore(&cli.config, args).await?;
            return Ok(());
        }
    }
    if matches!(&command, Commands::Init) {
        ensure!(
            is_system_profile_path(&cli.config),
            "repository initialization requires an installed /etc/boxup/HOST.toml profile in explicit new mode"
        );
    }
    if is_system_profile_path(&cli.config) {
        if let Commands::Key {
            command: KeyCommand::Export { to },
        } = &command
        {
            ensure!(
                to.is_none(),
                "system key export uses the fixed /etc/boxup/HOST.repokey destination"
            );
        }
    }
    if is_system_profile_path(&cli.config) {
        if matches!(&command, Commands::Backup { .. }) {
            let snapshot = invoke_root_backup(&cli.config).await?;
            println!("{}", snapshot.name);
            return Ok(());
        }
        if let Some(operation) = delegated_operation(&command) {
            invoke_root_operation(&cli.config, operation).await?;
            return Ok(());
        }
    }
    let config = Config::load(&cli.config)?;
    let backend = BorgBackend::new(&config);
    let checks_index = matches!(
        &command,
        Commands::Status { .. }
            | Commands::Snapshots { .. }
            | Commands::Ls { .. }
            | Commands::Search { .. }
            | Commands::Tui
    );
    let index_lock = (checks_index && !matches!(&command, Commands::Status { .. } | Commands::Tui))
        .then(|| LocalLock::acquire(&config.backup.state_dir, LockMode::Shared))
        .transpose()?;
    let index_usable = if checks_index && Index::exists(&config.index.path) {
        cached_index_is_usable(
            &config.index.path,
            &config.repository.location,
            INDEX_MAX_AGE,
        )
    } else {
        false
    };
    let requires_live_backend = match &command {
        Commands::Status { .. }
        | Commands::Search { .. }
        | Commands::Audit { .. }
        | Commands::Config { .. }
        | Commands::Automation { .. }
        | Commands::Notify { .. }
        | Commands::Setup
        | Commands::Tui => false,
        Commands::Restore(args) if args.overwrite => false,
        Commands::Snapshots { live, .. } | Commands::Ls { live, .. } => *live || !index_usable,
        _ => true,
    };
    if requires_live_backend {
        backend.preflight().await?;
    }

    match command {
        Commands::Setup => unreachable!("setup is handled before loading a profile"),
        Commands::Automation { command } => {
            run_automation(&config.host.id, command).await?;
        }
        Commands::Notify { .. } => {
            bail!("desktop notifications require an installed browse profile");
        }
        Commands::Init => {
            privilege_notice("init", &cli.config);
            let _lock = LocalLock::acquire(&config.backup.state_dir, LockMode::Exclusive)?;
            backend.init_repository().await?;
            println!("Repository initialized. Export the repokey now and store it separately.");
        }
        Commands::Backup { .. } => {
            privilege_notice("backup", &cli.config);
            let index = Index::open(&config.index.path)?;
            let renderer = Mutex::new(ProgressRenderer::new());
            let (cancel_sender, cancel_receiver) = tokio::sync::watch::channel(false);
            let mut signals = termination_signals()?;
            tokio::spawn(async move {
                if signals.recv().await.is_some() {
                    let _ = cancel_sender.send(true);
                }
            });
            let snapshot = JobRunner::new(&config, &backend, &index)
                .backup_with_progress_cancellable(
                    |progress| {
                        if let Ok(mut renderer) = renderer.lock() {
                            renderer.render(progress);
                        }
                    },
                    cancel_receiver,
                )
                .await?;
            println!("{}", snapshot.name);
        }
        Commands::Status { json } => {
            let status = load_status(
                config.host.id,
                config.repository.location,
                &config.index.path,
                &config.backup.state_dir,
                config.schedule.due_hours,
                index_usable,
            )?;
            print_status(status, json)?;
        }
        Commands::Snapshots { json, live } => {
            let snapshots = if !live && index_usable {
                match Index::open_read_only(&config.index.path).and_then(|index| index.snapshots())
                {
                    Ok(snapshots) => snapshots,
                    Err(error) => {
                        tracing::warn!(
                            "cached snapshots failed, falling back to live Borg: {error:#}"
                        );
                        backend.preflight().await?;
                        backend.list_snapshots().await?
                    }
                }
            } else {
                backend.list_snapshots().await?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else {
                for snapshot in snapshots {
                    println!("{}\t{}", snapshot.start.to_rfc3339(), snapshot.name);
                }
            }
        }
        Commands::Ls {
            snapshot,
            path,
            json,
            live,
        } => {
            let items = if !live && index_usable {
                match Index::open_read_only(&config.index.path)
                    .and_then(|index| index.list_files(&snapshot, path.as_deref()))
                {
                    Ok(items) => items,
                    Err(error) => {
                        tracing::warn!(
                            "cached file listing failed, falling back to live Borg: {error:#}"
                        );
                        backend.preflight().await?;
                        collect_live_files(&backend, &snapshot, path.as_deref()).await?
                    }
                }
            } else {
                collect_live_files(&backend, &snapshot, path.as_deref()).await?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                for item in items {
                    println!("{:?}\t{}\t{}", item.kind, item.size, item.path);
                }
            }
        }
        Commands::Search {
            query,
            all_snapshots,
            json,
        } => {
            ensure!(
                index_usable,
                "index is incomplete, stale, or for another repository; run 'boxup index refresh'"
            );
            let results =
                Index::open_read_only(&config.index.path)?.search(&query, all_snapshots)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in results {
                    println!("{}\t{}\t{}", result.snapshot, result.size, result.path);
                }
            }
        }
        Commands::Restore(args) => {
            if args.overwrite {
                invoke_root_restore(&cli.config, &args).await?;
            } else {
                ensure!(!args.sudo, "--sudo is valid only with --overwrite");
                let plan =
                    restore(&backend, &config, &args.snapshot, &args.paths, &args.to).await?;
                println!(
                    "Restored {} entries ({} bytes) to {}",
                    plan.files,
                    plan.bytes,
                    args.to.display()
                );
            }
        }
        Commands::Mount { snapshot, target } => {
            validate_mountpoint(&target)?;
            backend.mount(&snapshot, &target).await?;
            println!("Mounted {snapshot} at {}", target.display());
        }
        Commands::Umount { target } => {
            ensure!(
                fs::symlink_metadata(&target)?.is_dir(),
                "mountpoint must be a directory"
            );
            backend.umount(&target).await?;
        }
        Commands::Diff { a, b, path, json } => {
            let mut stream = backend.diff(&a, &b, path.as_deref()).await?;
            if json {
                let mut entries = Vec::new();
                while let Some(entry) = stream.try_next().await? {
                    entries.push(entry);
                }
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                while let Some(entry) = stream.try_next().await? {
                    println!("{}\t{}", entry.change, entry.path);
                }
            }
        }
        Commands::Prune { dry_run } => {
            privilege_notice("maintenance", &cli.config);
            let index = Index::open(&config.index.path)?;
            JobRunner::new(&config, &backend, &index)
                .prune(dry_run)
                .await?;
        }
        Commands::Check { verify_data } => {
            privilege_notice("check", &cli.config);
            let index = Index::open(&config.index.path)?;
            JobRunner::new(&config, &backend, &index)
                .check(verify_data)
                .await?;
        }
        Commands::Index {
            command: IndexCommand::Refresh,
        } => {
            let index = Index::open(&config.index.path)?;
            let stats = JobRunner::new(&config, &backend, &index)
                .refresh_index()
                .await?;
            println!(
                "Added {} archives and {} files; removed {} archives",
                stats.archives_added, stats.files_added, stats.archives_removed
            );
        }
        Commands::Config {
            command: ConfigCommand::Validate { system_profile },
        } => {
            if let Some(path) = system_profile {
                config.validate_system_profile(&path)?;
            }
            println!("Configuration is valid");
        }
        Commands::Config {
            command: ConfigCommand::BrowseDescriptor { system_profile },
        } => {
            let browse = BrowseConfig::from_system(&config, &system_profile)?;
            print!("{}", toml::to_string(&browse)?);
        }
        Commands::Config {
            command: ConfigCommand::CredentialRequirements,
        } => {
            println!(
                "{}",
                if config.notifications.enabled {
                    "discord_webhook"
                } else {
                    "none"
                }
            );
        }
        Commands::Key {
            command: KeyCommand::Export { to },
        } => {
            let destination = to.unwrap_or_else(|| {
                config
                    .backup
                    .state_dir
                    .join("recovery")
                    .join(format!("{}.repokey", config.host.id))
            });
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
                let mut permissions = fs::metadata(parent)?.permissions();
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(0o700);
                fs::set_permissions(parent, permissions)?;
            }
            backend.key_export(&destination).await?;
            println!("Exported repository key to {}", destination.display());
        }
        Commands::Audit {
            command: AuditCommand::Docker { json, running },
        } => {
            let audit = DockerManager::new(&config).audit(!running).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&audit)?);
            } else if !audit.available {
                println!("Docker integration is disabled");
            } else {
                println!("Containers: {}", audit.containers.len());
                println!("Compose projects: {}", audit.compose_projects.join(", "));
                for container in audit.containers {
                    println!(
                        "{}\t{}\trunning={}\tstateful={}\tpostgres={}",
                        container.name,
                        container.image,
                        container.running,
                        container.stateful,
                        container.postgres
                    );
                }
            }
        }
        Commands::Tui => {
            drop(index_lock);
            let index = if Index::exists(&config.index.path) {
                Index::open_read_only(&config.index.path)?
            } else {
                Index::open(&config.index.path)?
            };
            boxup::tui::run(
                &index,
                boxup::tui::TuiContext {
                    config_path: cli.config,
                    host: config.host.id.clone(),
                    repository: config.repository.location,
                    state_dir: config.backup.state_dir,
                    due_hours: config.schedule.due_hours,
                    automatic_backups: Some(backup_timer_enabled(&config.host.id)),
                    background_indexing: Some(index_timer_enabled(&config.host.id)),
                },
            )?;
        }
    }
    Ok(())
}

async fn run_browse(command: Commands, browse: BrowseConfig) -> Result<()> {
    if let Commands::Automation { command } = &command {
        return run_automation(&browse.host, *command).await;
    }
    if let Commands::Notify { watch, state_file } = &command {
        ensure!(*watch, "notification mode requires --watch");
        return watch_notifications(&browse, state_file.as_deref()).await;
    }
    if matches!(&command, Commands::Backup { .. }) {
        let snapshot = invoke_root_backup(&browse.system_profile).await?;
        println!("{}", snapshot.name);
        return Ok(());
    }
    let lock = (!matches!(&command, Commands::Status { .. } | Commands::Tui))
        .then(|| LocalLock::acquire(&browse.state_dir, LockMode::Shared))
        .transpose()?;
    let index_exists = Index::exists(&browse.index_path);
    let index_usable = if index_exists {
        cached_index_is_usable(
            &browse.index_path,
            &browse.repository_location,
            INDEX_MAX_AGE,
        )
    } else {
        false
    };
    match command {
        Commands::Status { json } => {
            let status = load_status(
                browse.host,
                browse.repository_location,
                &browse.index_path,
                &browse.state_dir,
                browse.due_hours,
                index_usable,
            )?;
            print_status(status, json)?;
        }
        Commands::Snapshots { json, live } => {
            if live || !index_usable {
                invoke_root_operation(&browse.system_profile, RootOperation::Snapshots { json })
                    .await?;
            } else {
                match Index::open_read_only(&browse.index_path).and_then(|index| index.snapshots())
                {
                    Ok(snapshots) => {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&snapshots)?);
                        } else {
                            for snapshot in snapshots {
                                println!("{}\t{}", snapshot.start.to_rfc3339(), snapshot.name);
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            "cached snapshots failed, falling back to live Borg: {error:#}"
                        );
                        invoke_root_operation(
                            &browse.system_profile,
                            RootOperation::Snapshots { json },
                        )
                        .await?;
                    }
                }
            }
        }
        Commands::Ls {
            snapshot,
            path,
            json,
            live,
        } => {
            if live || !index_usable {
                invoke_root_operation(
                    &browse.system_profile,
                    RootOperation::Ls {
                        snapshot,
                        path,
                        json,
                    },
                )
                .await?;
            } else {
                match Index::open_read_only(&browse.index_path)
                    .and_then(|index| index.list_files(&snapshot, path.as_deref()))
                {
                    Ok(items) => {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&items)?);
                        } else {
                            for item in items {
                                println!("{:?}\t{}\t{}", item.kind, item.size, item.path);
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            "cached file listing failed, falling back to live Borg: {error:#}"
                        );
                        invoke_root_operation(
                            &browse.system_profile,
                            RootOperation::Ls {
                                snapshot,
                                path,
                                json,
                            },
                        )
                        .await?;
                    }
                }
            }
        }
        Commands::Search {
            query,
            all_snapshots,
            json,
        } => {
            ensure!(
                index_usable,
                "browse index is incomplete, mismatched, or stale"
            );
            let results =
                Index::open_read_only(&browse.index_path)?.search(&query, all_snapshots)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in results {
                    println!("{}\t{}\t{}", result.snapshot, result.size, result.path);
                }
            }
        }
        Commands::Index {
            command: IndexCommand::Refresh,
        } => {
            drop(lock);
            invoke_root_operation(&browse.system_profile, RootOperation::IndexRefresh).await?;
        }
        Commands::Tui => {
            drop(lock);
            ensure!(index_exists, "browse index is unavailable");
            boxup::tui::run(
                &Index::open_read_only(&browse.index_path)?,
                boxup::tui::TuiContext {
                    config_path: browse.system_profile,
                    host: browse.host.clone(),
                    repository: browse.repository_location,
                    state_dir: browse.state_dir,
                    due_hours: browse.due_hours,
                    automatic_backups: Some(backup_timer_enabled(&browse.host)),
                    background_indexing: Some(index_timer_enabled(&browse.host)),
                },
            )?;
        }
        Commands::Automation { .. } => unreachable!("automation is handled before index access"),
        Commands::Notify { .. } => unreachable!("notifications are handled before index access"),
        Commands::Setup => unreachable!("setup is handled before browse profile discovery"),
        _ => bail!(
            "--browse-config supports only backup, automation, status, snapshots, ls, search, index refresh, and tui"
        ),
    }
    Ok(())
}

fn cached_index_is_usable(path: &Path, repository_location: &str, max_age: Duration) -> bool {
    match Index::open_read_only(path)
        .and_then(|index| index.is_usable(repository_location, max_age))
    {
        Ok(usable) => usable,
        Err(error) => {
            tracing::warn!(index = %path.display(), "cached index is unavailable: {error:#}");
            false
        }
    }
}

fn load_status(
    host: String,
    repository: String,
    index_path: &Path,
    state_dir: &Path,
    due_hours: u64,
    index_usable: bool,
) -> Result<StatusOutput> {
    let (
        index,
        last_backup_details,
        latest_backup_attempt,
        running_jobs,
        jobs,
        estimated_backup_seconds,
    ) = if Index::exists(index_path) {
        let index = Index::open_read_only(index_path)?;
        (
            index.status()?,
            index.last_success_job("backup")?,
            index.latest_job("backup")?,
            index.running_jobs()?,
            index.recent_jobs(10)?,
            index.estimated_backup_seconds(5)?,
        )
    } else {
        (
            IndexStatus::default(),
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
        )
    };
    let last_backup = last_backup_details.as_ref().and_then(|job| job.finished_at);
    let due_after = chrono::Duration::hours(
        i64::try_from(due_hours).context("schedule due interval is too large")?,
    );
    let next_due = last_backup.map(|last| last + due_after);
    let due = next_due.is_none_or(|next| utc_now() >= next);
    Ok(StatusOutput {
        host,
        repository,
        last_backup,
        last_backup_details,
        due,
        next_due,
        index,
        index_usable,
        operation_active: LocalLock::is_held(state_dir)?,
        estimated_backup_seconds,
        latest_backup_attempt,
        running_jobs,
        jobs,
    })
}

fn print_status(status: StatusOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    let running = status.running_jobs.first();
    let latest_backup = status.latest_backup_attempt.as_ref();
    let health = if latest_backup.is_some_and(|job| {
        job.state == boxup::domain::JobState::Running
            && status.operation_active
            && running.is_some_and(|running| running.id == job.id)
    }) {
        "running"
    } else if latest_backup.is_some_and(|job| job.state == boxup::domain::JobState::Running) {
        "stale"
    } else if latest_backup.is_some_and(|job| job.state == boxup::domain::JobState::Failed) {
        "last attempt failed"
    } else if status.last_backup.is_some() {
        "healthy"
    } else {
        "no successful backup"
    };
    println!("Host: {}", status.host);
    println!("Repository: {}", status.repository);
    println!("Backup: {health}");
    println!(
        "Last success: {}",
        status
            .last_backup
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "never".into())
    );
    if let Some(job) = &status.last_backup_details {
        if let Some(archive) = &job.archive_name {
            println!("Archive: {archive}");
        }
        if let Some(finished) = job.finished_at {
            let seconds = finished
                .signed_duration_since(job.started_at)
                .num_seconds()
                .max(0) as u64;
            println!("Duration: {}", human_duration(seconds));
        }
        if job.stats_recorded {
            println!(
                "Data: {} logical, {} compressed, {} new, {} files",
                human_bytes(job.original_bytes),
                human_bytes(job.compressed_bytes),
                human_bytes(job.deduplicated_bytes),
                job.files
            );
        } else {
            println!("Data: unknown (backup predates statistics tracking)");
        }
    }
    println!(
        "Next due: {}{}",
        status
            .next_due
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "now".into()),
        if status.due { " (due)" } else { "" }
    );
    if let Some(job) = running {
        println!(
            "Current job: {} ({})  {} files  {} processed",
            job.phase.as_deref().unwrap_or("starting"),
            if status.operation_active {
                "active"
            } else {
                "stale"
            },
            job.files,
            human_bytes(job.original_bytes)
        );
    } else if status.operation_active {
        println!("Current job: operation active (details not yet recorded)");
    }
    if let Some(estimate) = status.estimated_backup_seconds {
        println!("Historical estimate: ~{}", human_duration(estimate));
    } else {
        println!("Historical estimate: unavailable (need 3 successful runs)");
    }
    println!(
        "Index: {}{}",
        if status.index.complete {
            "complete"
        } else {
            "incomplete"
        },
        if status.index_usable {
            ", usable"
        } else {
            ", not usable"
        }
    );
    println!(
        "Index refreshed: {}",
        status
            .index
            .refreshed_at
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "never".into())
    );
    if !status.jobs.is_empty() {
        println!("Recent jobs:");
        for job in status.jobs.iter().take(5) {
            let duration = job.finished_at.map(|finished| {
                human_duration(
                    finished
                        .signed_duration_since(job.started_at)
                        .num_seconds()
                        .max(0) as u64,
                )
            });
            println!(
                "  {}  {:?}  {}{}",
                job.started_at.to_rfc3339(),
                job.state,
                job.kind,
                duration.map_or_else(String::new, |value| format!("  {value}"))
            );
            if let Some(message) = &job.message {
                println!("  {message}");
            }
        }
    }
    Ok(())
}

struct ProgressRenderer {
    interactive: bool,
    last_phase: Option<BackupPhase>,
    last_log: Instant,
}

impl ProgressRenderer {
    fn new() -> Self {
        Self {
            interactive: std::io::stderr().is_terminal(),
            last_phase: None,
            last_log: Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now),
        }
    }

    fn render(&mut self, progress: BackupProgress) {
        let phase_changed = self.last_phase != Some(progress.phase);
        let terminal = matches!(progress.phase, BackupPhase::Complete | BackupPhase::Failed);
        if !self.interactive && !phase_changed && self.last_log.elapsed() < Duration::from_secs(30)
        {
            return;
        }

        let line = format_backup_progress(progress);
        if self.interactive {
            eprint!("\r\x1b[2K{line}");
            if terminal {
                eprintln!();
            }
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{line}");
        }
        self.last_phase = Some(progress.phase);
        self.last_log = Instant::now();
    }
}

fn format_backup_progress(progress: BackupProgress) -> String {
    let phase = match progress.phase {
        BackupPhase::Preparing => "Preparing",
        BackupPhase::Auditing => "Auditing",
        BackupPhase::Staging => "Staging application data",
        BackupPhase::CreatingArchive => "Creating archive",
        BackupPhase::RefreshingIndex => "Refreshing index",
        BackupPhase::Finalizing => "Finalizing",
        BackupPhase::Complete => "Backup complete",
        BackupPhase::Failed => "Backup failed",
    };
    let (bar, estimate) = estimated_bar(progress);
    let mut details = format!(
        "{} files  {} processed  {} new",
        progress.files,
        human_bytes(progress.original_bytes),
        human_bytes(progress.deduplicated_bytes)
    );
    if progress.elapsed_seconds > 0 && progress.original_bytes > 0 {
        details.push_str(&format!(
            "  {}/s",
            human_bytes(progress.original_bytes / progress.elapsed_seconds)
        ));
    }
    format!(
        "{phase:<24} [{bar}] {estimate}  elapsed {}  {details}",
        human_duration(progress.elapsed_seconds)
    )
}

fn estimated_bar(progress: BackupProgress) -> (String, String) {
    const WIDTH: usize = 18;
    if matches!(progress.phase, BackupPhase::Complete) {
        return ("=".repeat(WIDTH), "100%".into());
    }
    if let Some(total) = progress.estimated_total_seconds.filter(|total| *total > 0) {
        let ratio = (progress.elapsed_seconds as f64 / total as f64).min(0.95);
        let filled = (ratio * WIDTH as f64).floor() as usize;
        let bar = format!("{}{}", "=".repeat(filled), ".".repeat(WIDTH - filled));
        let estimate = if progress.elapsed_seconds < total {
            format!(
                "~{:.0}%  ETA ~{}",
                ratio * 100.0,
                human_duration(total - progress.elapsed_seconds)
            )
        } else {
            "~95%  longer than estimate".into()
        };
        return (bar, estimate);
    }
    let position = (progress.elapsed_seconds as usize) % WIDTH;
    let mut bar = vec!['.'; WIDTH];
    bar[position] = '=';
    (
        bar.into_iter().collect(),
        "estimating (need 3 successful runs)".into(),
    )
}

fn human_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

async fn collect_live_files(
    backend: &BorgBackend,
    snapshot: &str,
    path: Option<&str>,
) -> Result<Vec<boxup::domain::ArchiveItem>> {
    let mut stream = backend.list_files(snapshot, path).await?;
    let mut items = Vec::new();
    while let Some(item) = stream.try_next().await? {
        items.push(item);
    }
    Ok(items)
}

fn expand_config_path(path: &mut PathBuf) -> Result<()> {
    if path == Path::new("~") || path.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is unavailable")?;
        *path = home.join(path.strip_prefix("~").expect("prefix checked"));
    }
    ensure!(path.is_absolute(), "config path must be absolute");
    Ok(())
}

fn supports_auto_browse(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Backup { .. }
            | Commands::Automation { .. }
            | Commands::Notify { .. }
            | Commands::Status { .. }
            | Commands::Snapshots { .. }
            | Commands::Ls { .. }
            | Commands::Search { .. }
            | Commands::Index {
                command: IndexCommand::Refresh,
            }
            | Commands::Tui
    )
}

fn discover_browse_config() -> Result<Option<PathBuf>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable")?;
    discover_browse_config_in(&home.join(".config/boxup"))
}

fn discover_browse_config_in(directory: &Path) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to inspect browse config directory"),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_file()
            && !metadata.file_type().is_symlink()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("-browse.toml"))
        {
            candidates.push(path);
        }
    }
    candidates.sort();
    ensure!(
        candidates.len() <= 1,
        "multiple browse profiles found; select one with --browse-config"
    );
    Ok(candidates.pop())
}

async fn invoke_root_backup(config: &Path) -> Result<Snapshot> {
    let mut signals = termination_signals()?;
    let mut cancellation_reported = false;
    let mut command = if nix::unistd::Uid::effective().is_root() {
        Command::new("/usr/lib/boxup/boxup-root")
    } else {
        let mut command = Command::new("/usr/bin/sudo");
        command.arg("-n");
        command.arg("/usr/lib/boxup/boxup-root");
        command
    };
    let mut child = command
        .arg("--config")
        .arg(config)
        .arg("backup")
        .arg("--progress-json")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start fixed privileged backup helper")?;
    let mut control = Some(
        child
            .stdin
            .take()
            .context("backup helper control pipe was unavailable")?,
    );
    let stdout = child
        .stdout
        .take()
        .context("backup helper stdout was unavailable")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut renderer = ProgressRenderer::new();
    let mut snapshot = None;
    let mut complete = false;
    'progress: loop {
        let line = tokio::select! {
            line = lines.next_line() => line?,
            signal = signals.recv(), if !complete => {
                if signal.is_none() {
                    continue;
                }
                if !cancellation_reported {
                    cancellation_reported = true;
                    eprintln!("Cancellation requested; waiting until application data is safely resumed...");
                }
                drop(control.take());
                break 'progress;
            }
        };
        let Some(line) = line else {
            break;
        };
        let event: BackupProtocolEvent = serde_json::from_str(&line)
            .context("privileged backup helper emitted invalid progress data")?;
        ensure!(event.version == 1, "unsupported backup progress protocol");
        match event.event {
            BackupProtocolEventKind::Progress { progress } => {
                complete = progress.phase == BackupPhase::Complete;
                renderer.render(progress);
            }
            BackupProtocolEventKind::Result { snapshot: result } => snapshot = Some(result),
        }
    }
    drop(lines);
    drop(control.take());
    let status = child
        .wait()
        .await
        .context("failed to wait for fixed privileged backup helper")?;
    if cancellation_reported {
        bail!("backup cancelled");
    }
    ensure!(
        status.success(),
        "backup did not complete; open 'boxup' for status and details"
    );
    snapshot.context("privileged backup helper returned no result")
}

fn termination_signals() -> Result<tokio::sync::mpsc::UnboundedReceiver<nix::sys::signal::Signal>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let signal = tokio::select! {
                value = interrupt.recv() => value.map(|_| nix::sys::signal::Signal::SIGINT),
                value = terminate.recv() => value.map(|_| nix::sys::signal::Signal::SIGTERM),
                value = hangup.recv() => value.map(|_| nix::sys::signal::Signal::SIGHUP),
            };
            let Some(signal) = signal else {
                break;
            };
            if sender.send(signal).is_err() {
                break;
            }
        }
    });
    Ok(receiver)
}

fn privilege_notice(operation: &str, config: &Path) {
    if !nix::unistd::Uid::effective().is_root() {
        tracing::info!(
            "running {operation} unprivileged; /etc/boxup profiles are delegated to fixed boxup-root operations automatically"
        );
        tracing::debug!(config = %config.display(), "selected config");
    }
}

async fn watch_notifications(browse: &BrowseConfig, state_file: Option<&Path>) -> Result<()> {
    let state_path = if let Some(path) = state_file {
        ensure!(
            path.is_absolute(),
            "notification state path must be absolute"
        );
        path.to_path_buf()
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is unavailable for desktop notifications")?;
        let state_dir = home.join(".local/state/boxup");
        fs::create_dir_all(&state_dir)?;
        state_dir.join(format!("{}-notified-job", browse.host))
    };
    let mut last_notified = fs::read_to_string(&state_path)
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok());
    loop {
        if let Ok(index) = Index::open_read_only(&browse.index_path) {
            if let Ok(Some(job)) = index.latest_job("backup") {
                if let Some(previous) = last_notified {
                    if job.id > previous && job.state != boxup::domain::JobState::Running {
                        send_desktop_notification(&job).await;
                        write_notification_state(&state_path, job.id)?;
                        last_notified = Some(job.id);
                    }
                } else {
                    write_notification_state(&state_path, job.id)?;
                    last_notified = Some(job.id);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

async fn send_desktop_notification(job: &boxup::domain::JobRecord) {
    let (title, body) = notification_text(job);
    let result = Command::new("/usr/bin/notify-send")
        .arg("--app-name=Boxup")
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if !result.is_ok_and(|status| status.success()) {
        tracing::warn!("could not deliver Boxup desktop notification");
    }
}

fn notification_text(job: &boxup::domain::JobRecord) -> (&'static str, &'static str) {
    match job.state {
        boxup::domain::JobState::Succeeded if job.message.is_some() => (
            "Backup completed",
            "Your computer is protected. One small note was recorded.",
        ),
        boxup::domain::JobState::Succeeded => ("Backup completed", "Your computer is protected."),
        boxup::domain::JobState::Failed => (
            "Backup needs attention",
            "The latest backup did not complete. Open Boxup for details.",
        ),
        boxup::domain::JobState::Running => ("Backup running", "Boxup is creating a backup."),
    }
}

fn write_notification_state(path: &Path, job: i64) -> Result<()> {
    fs::write(path, format!("{job}\n"))?;
    let mut permissions = fs::metadata(path)?.permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

async fn run_automation(host: &str, command: AutomationCommand) -> Result<()> {
    match command {
        AutomationCommand::Status { json } => {
            let backup = backup_timer_enabled(host);
            let index = index_timer_enabled(host);
            let notifications = if nix::unistd::Uid::effective().is_root() {
                None
            } else {
                Some(user_notification_state(host).await?.0)
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "automatic_backups": backup,
                        "background_indexing": index,
                        "desktop_notifications": notifications,
                    }))?
                );
            } else {
                println!(
                    "Automatic backups: {}\nBackground indexing: {}\nDesktop notifications: {}",
                    if backup { "On" } else { "Off" },
                    if index { "On" } else { "Off" },
                    notifications.map_or("Unavailable", |enabled| if enabled {
                        "On"
                    } else {
                        "Off"
                    })
                );
            }
            return Ok(());
        }
        AutomationCommand::Enable | AutomationCommand::Disable => {}
    }

    let action = match command {
        AutomationCommand::Enable => "enable",
        AutomationCommand::Disable => "disable",
        AutomationCommand::Status { .. } => unreachable!(),
    };
    ensure!(
        !nix::unistd::Uid::effective().is_root(),
        "run this command as the desktop user; it requests administrator authentication when needed"
    );
    let previous_notifications = user_notification_state(host).await?;
    if let Err(error) = set_user_notification_state(
        host,
        matches!(command, AutomationCommand::Enable),
        matches!(command, AutomationCommand::Enable),
    )
    .await
    {
        restore_user_notification_state(host, previous_notifications)
            .await
            .context("failed to restore desktop notifications after automation error")?;
        return Err(error);
    }
    let mut child = Command::new("/usr/bin/pkexec");
    child.arg("/usr/lib/boxup/setup-automation");
    let status = match child
        .arg(host)
        .arg(action)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
    {
        Ok(status) => status,
        Err(error) => {
            restore_user_notification_state(host, previous_notifications)
                .await
                .context("failed to restore desktop notifications after automation error")?;
            return Err(error).context("failed to change Boxup automation");
        }
    };
    if !status.success() {
        restore_user_notification_state(host, previous_notifications)
            .await
            .context("failed to restore desktop notifications after automation failure")?;
        bail!("could not change Boxup automation");
    }
    Ok(())
}

async fn user_notification_state(host: &str) -> Result<(bool, bool)> {
    boxup::config::validate_id("host", host)?;
    let unit = format!("boxup-notify@{host}.service");
    let enabled = user_unit_status("is-enabled", &unit).await?;
    let active = user_unit_status("is-active", &unit).await?;
    Ok((enabled, active))
}

async fn user_unit_status(action: &str, unit: &str) -> Result<bool> {
    let status = Command::new("/usr/bin/systemctl")
        .args(["--user", action, "--quiet", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("failed to inspect Boxup desktop notifications")?;
    match (action, status.code()) {
        (_, Some(0)) => Ok(true),
        ("is-enabled", Some(1)) | ("is-active", Some(3)) => Ok(false),
        _ => bail!("systemd could not determine desktop notification state"),
    }
}

async fn set_user_notification_state(host: &str, enabled: bool, active: bool) -> Result<()> {
    boxup::config::validate_id("host", host)?;
    let unit = format!("boxup-notify@{host}.service");
    run_user_systemctl(if enabled { "enable" } else { "disable" }, &unit).await?;
    run_user_systemctl(if active { "start" } else { "stop" }, &unit).await?;
    Ok(())
}

async fn restore_user_notification_state(host: &str, expected: (bool, bool)) -> Result<()> {
    set_user_notification_state(host, expected.0, expected.1).await?;
    ensure!(
        user_notification_state(host).await? == expected,
        "desktop notification rollback did not restore its previous state"
    );
    Ok(())
}

async fn run_user_systemctl(action: &str, unit: &str) -> Result<()> {
    let status = Command::new("/usr/bin/systemctl")
        .args(["--user", action, unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("failed to manage Boxup desktop notifications")?;
    ensure!(status.success(), "could not manage desktop notifications");
    Ok(())
}

fn backup_timer_enabled(host: &str) -> bool {
    unit_enabled(&format!("boxup-backup-desktop@{host}.timer"))
        || unit_enabled(&format!("boxup-backup-server@{host}.timer"))
}

fn index_timer_enabled(host: &str) -> bool {
    unit_enabled(&format!("boxup-index@{host}.timer"))
}

fn unit_enabled(unit: &str) -> bool {
    std::process::Command::new("/usr/bin/systemctl")
        .args(["is-enabled", "--quiet", unit])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[derive(Clone)]
enum RootOperation {
    Init,
    Maintenance {
        dry_run: bool,
    },
    Check {
        verify_data: bool,
    },
    IndexRefresh,
    KeyExport,
    Snapshots {
        json: bool,
    },
    Ls {
        snapshot: String,
        path: Option<String>,
        json: bool,
    },
}

fn is_system_profile_path(path: &Path) -> bool {
    path.parent() == Some(Path::new("/etc/boxup"))
        && path
            .extension()
            .is_some_and(|extension| extension == "toml")
}

fn delegated_operation(command: &Commands) -> Option<RootOperation> {
    match command {
        Commands::Init => Some(RootOperation::Init),
        Commands::Prune { dry_run } => Some(RootOperation::Maintenance { dry_run: *dry_run }),
        Commands::Check { verify_data } => Some(RootOperation::Check {
            verify_data: *verify_data,
        }),
        Commands::Index {
            command: IndexCommand::Refresh,
        } => Some(RootOperation::IndexRefresh),
        Commands::Key {
            command: KeyCommand::Export { .. },
        } => Some(RootOperation::KeyExport),
        Commands::Snapshots { json, .. } => Some(RootOperation::Snapshots { json: *json }),
        Commands::Ls {
            snapshot,
            path,
            json,
            ..
        } => Some(RootOperation::Ls {
            snapshot: snapshot.clone(),
            path: path.clone(),
            json: *json,
        }),
        _ => None,
    }
}

async fn invoke_root_operation(config: &Path, operation: RootOperation) -> Result<()> {
    let mut command = if nix::unistd::Uid::effective().is_root() {
        Command::new("/usr/lib/boxup/boxup-root")
    } else {
        let mut command = Command::new("/usr/bin/pkexec");
        command.arg("/usr/lib/boxup/boxup-root");
        command
    };
    command.arg("--config").arg(config);
    match operation {
        RootOperation::Init => {
            command.arg("init");
        }
        RootOperation::Maintenance { dry_run } => {
            command.arg("maintenance");
            if dry_run {
                command.arg("--dry-run");
            }
        }
        RootOperation::Check { verify_data } => {
            command.arg("check");
            if verify_data {
                command.arg("--verify-data");
            }
        }
        RootOperation::IndexRefresh => {
            command.arg("index-refresh");
        }
        RootOperation::KeyExport => {
            command.arg("key-export");
        }
        RootOperation::Snapshots { json } => {
            command.arg("snapshots");
            if json {
                command.arg("--json");
            }
        }
        RootOperation::Ls {
            snapshot,
            path,
            json,
        } => {
            command.arg("ls").arg(snapshot);
            if let Some(path) = path {
                command.arg(path);
            }
            if json {
                command.arg("--json");
            }
        }
    }
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("failed to execute fixed privileged helper")?;
    ensure!(status.success(), "privileged operation failed");
    Ok(())
}

async fn invoke_root_restore(config: &Path, args: &RestoreArgs) -> Result<()> {
    ensure!(
        config.starts_with("/etc/boxup"),
        "privileged restore requires a profile under /etc/boxup"
    );
    let mut command = if nix::unistd::Uid::effective().is_root() {
        Command::new("/usr/lib/boxup/boxup-root")
    } else {
        let mut command = Command::new("/usr/bin/pkexec");
        command.arg("/usr/lib/boxup/boxup-root");
        command
    };
    command
        .arg("--config")
        .arg(config)
        .arg("restore-overwrite")
        .arg(&args.snapshot)
        .args(&args.paths)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command
        .status()
        .await
        .context("failed to execute fixed privileged helper")?;
    ensure!(status.success(), "privileged restore failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_cli_accepts_literal_option_shaped_paths_after_delimiter() {
        let cli = Cli::try_parse_from([
            "boxup",
            "restore",
            "--to",
            "/restore",
            "host-archive",
            "--",
            "-literal",
        ])
        .unwrap();
        let Some(Commands::Restore(args)) = cli.command else {
            panic!("restore command was not parsed");
        };
        assert_eq!(args.paths, ["-literal"]);
    }

    #[test]
    fn backup_run_subcommand_is_optional() {
        let short = Cli::try_parse_from(["boxup", "backup"]).unwrap();
        let Some(Commands::Backup { command }) = short.command else {
            panic!("backup command was not parsed");
        };
        assert!(command.is_none());

        let legacy = Cli::try_parse_from(["boxup", "backup", "run"]).unwrap();
        let Some(Commands::Backup { command }) = legacy.command else {
            panic!("backup run command was not parsed");
        };
        assert!(matches!(command, Some(BackupCommand::Run)));
    }

    #[test]
    fn index_refresh_supports_automatic_browse_profile_discovery() {
        let cli = Cli::try_parse_from(["boxup", "index", "refresh"]).unwrap();
        assert!(supports_auto_browse(cli.command.as_ref().unwrap()));
    }

    #[test]
    fn no_subcommand_opens_the_terminal_app() {
        let cli = Cli::try_parse_from(["boxup"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn discovers_only_one_regular_browse_profile() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("boxup");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("ignored.toml"), "").unwrap();
        let expected = directory.join("desktop-browse.toml");
        fs::write(&expected, "").unwrap();
        assert_eq!(
            discover_browse_config_in(&directory).unwrap(),
            Some(expected)
        );

        fs::write(directory.join("server-browse.toml"), "").unwrap();
        let error = discover_browse_config_in(&directory).unwrap_err();
        assert!(error.to_string().contains("multiple browse profiles"));
    }

    #[test]
    fn malformed_index_is_not_considered_usable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite3");
        fs::write(&path, "not sqlite").unwrap();
        assert!(!cached_index_is_usable(&path, "/repository", INDEX_MAX_AGE));
    }
}
