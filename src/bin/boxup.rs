use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use boxup::backend::Backend;
use boxup::config::BrowseConfig;
use boxup::domain::utc_now;
use boxup::index::{Index, IndexStatus};
use boxup::jobs::{DockerManager, JobRunner, LocalLock, LockMode};
use boxup::restore::{restore, validate_mountpoint};
use boxup::{BorgBackend, Config};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use futures::TryStreamExt;
use serde::Serialize;
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
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
    due: bool,
    index: IndexStatus,
    index_usable: bool,
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
    let default_config_selected = cli.config == Path::new("~/.config/boxup/config.toml");
    expand_config_path(&mut cli.config)?;
    if cli.browse_config.is_none()
        && default_config_selected
        && !cli.config.exists()
        && supports_auto_browse(&cli.command)
    {
        cli.browse_config = discover_browse_config()?;
    }
    if let Some(path) = &mut cli.browse_config {
        expand_config_path(path)?;
        let browse = BrowseConfig::load(path)?;
        return run_browse(cli.command, browse).await;
    }
    if let Commands::Restore(args) = &cli.command {
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
    if is_system_profile_path(&cli.config) {
        if let Commands::Key {
            command: KeyCommand::Export { to },
        } = &cli.command
        {
            ensure!(
                to.is_none(),
                "system key export uses the fixed /etc/boxup/HOST.repokey destination"
            );
        }
    }
    if is_system_profile_path(&cli.config) {
        if let Some(operation) = delegated_operation(&cli.command) {
            invoke_root_operation(&cli.config, operation).await?;
            return Ok(());
        }
    }
    let config = Config::load(&cli.config)?;
    let backend = BorgBackend::new(&config);
    let checks_index = matches!(
        &cli.command,
        Commands::Status { .. }
            | Commands::Snapshots { .. }
            | Commands::Ls { .. }
            | Commands::Search { .. }
            | Commands::Tui
    );
    let index_lock = checks_index
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
    let requires_live_backend = match &cli.command {
        Commands::Status { .. }
        | Commands::Search { .. }
        | Commands::Audit { .. }
        | Commands::Config { .. }
        | Commands::Tui => false,
        Commands::Restore(args) if args.overwrite => false,
        Commands::Snapshots { live, .. } | Commands::Ls { live, .. } => *live || !index_usable,
        _ => true,
    };
    if requires_live_backend {
        backend.preflight().await?;
    }

    match cli.command {
        Commands::Init => {
            privilege_notice("init", &cli.config);
            let _lock = LocalLock::acquire(&config.backup.state_dir, LockMode::Exclusive)?;
            backend.init_repository().await?;
            println!("Repository initialized. Export the repokey now and store it separately.");
        }
        Commands::Backup { .. } => {
            privilege_notice("backup", &cli.config);
            let index = Index::open(&config.index.path)?;
            let snapshot = JobRunner::new(&config, &backend, &index).backup().await?;
            println!("{}", snapshot.name);
        }
        Commands::Status { json } => {
            let (index_status, last_backup, jobs) = if Index::exists(&config.index.path) {
                let index = Index::open_read_only(&config.index.path)?;
                (
                    index.status()?,
                    index.last_success("backup")?,
                    index.recent_jobs(10)?,
                )
            } else {
                (IndexStatus::default(), None, Vec::new())
            };
            let due = last_backup.is_none_or(|last| {
                utc_now() - last >= chrono::Duration::hours(config.schedule.due_hours as i64)
            });
            let status = StatusOutput {
                host: config.host.id,
                repository: config.repository.location,
                last_backup,
                due,
                index: index_status,
                index_usable,
                jobs,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Host: {}", status.host);
                println!("Repository: {}", status.repository);
                println!(
                    "Last successful backup: {}",
                    status
                        .last_backup
                        .map(|value| value.to_rfc3339())
                        .unwrap_or_else(|| "never".into())
                );
                println!("Due: {}", status.due);
                println!("Index complete: {}", status.index.complete);
                println!("Index usable: {}", status.index_usable);
                println!(
                    "Index refreshed: {}",
                    status
                        .index
                        .refreshed_at
                        .map(|value| value.to_rfc3339())
                        .unwrap_or_else(|| "never".into())
                );
                for job in status.jobs {
                    println!("{} {:?} {}", job.started_at, job.state, job.kind);
                }
            }
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
            ensure!(
                index_usable,
                "index is incomplete, stale, or for another repository; run 'boxup index refresh'"
            );
            drop(index_lock);
            boxup::tui::run(&Index::open_read_only(&config.index.path)?, &cli.config)?;
        }
    }
    Ok(())
}

async fn run_browse(command: Commands, browse: BrowseConfig) -> Result<()> {
    if matches!(&command, Commands::Backup { .. }) {
        return start_system_backup(&browse).await;
    }
    let lock = (!matches!(&command, Commands::Status { .. }))
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
            let (index_status, last_backup, jobs) = if index_exists {
                let index = Index::open_read_only(&browse.index_path)?;
                (
                    index.status()?,
                    index.last_success("backup")?,
                    index.recent_jobs(10)?,
                )
            } else {
                (IndexStatus::default(), None, Vec::new())
            };
            let due = last_backup.is_none_or(|last| {
                utc_now() - last >= chrono::Duration::hours(browse.due_hours as i64)
            });
            let status = StatusOutput {
                host: browse.host,
                repository: browse.repository_location,
                last_backup,
                due,
                index: index_status,
                index_usable,
                jobs,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Host: {}", status.host);
                println!("Repository: {}", status.repository);
                println!(
                    "Last successful backup: {}",
                    status
                        .last_backup
                        .map(|value| value.to_rfc3339())
                        .unwrap_or_else(|| "never".into())
                );
                println!("Due: {}", status.due);
                println!("Index complete: {}", status.index.complete);
                println!("Index usable: {}", status.index_usable);
            }
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
        Commands::Tui => {
            ensure!(
                index_usable,
                "browse index is incomplete, mismatched, or stale"
            );
            drop(lock);
            boxup::tui::run(
                &Index::open_read_only(&browse.index_path)?,
                &browse.system_profile,
            )?;
        }
        _ => bail!("--browse-config supports only backup, status, snapshots, ls, search, and tui"),
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
            | Commands::Status { .. }
            | Commands::Snapshots { .. }
            | Commands::Ls { .. }
            | Commands::Search { .. }
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

async fn start_system_backup(browse: &BrowseConfig) -> Result<()> {
    let unit = format!("boxup-backup-now@{}.service", browse.host);
    let mut command = if nix::unistd::Uid::effective().is_root() {
        Command::new("/usr/bin/systemctl")
    } else {
        let mut command = Command::new("/usr/bin/pkexec");
        command.arg("/usr/bin/systemctl");
        command
    };
    let status = command
        .arg("--no-block")
        .arg("start")
        .arg(&unit)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("failed to start fixed Boxup backup service")?;
    ensure!(status.success(), "failed to start {unit}");
    println!("Started {unit}. Follow it with: journalctl -fu {unit}");
    Ok(())
}

fn privilege_notice(operation: &str, config: &Path) {
    if !nix::unistd::Uid::effective().is_root() {
        tracing::info!(
            "running {operation} unprivileged; /etc/boxup profiles are delegated to fixed boxup-root operations automatically"
        );
        tracing::debug!(config = %config.display(), "selected config");
    }
}

#[derive(Clone)]
enum RootOperation {
    Init,
    Backup,
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
        Commands::Backup { .. } => Some(RootOperation::Backup),
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
        RootOperation::Backup => {
            command.arg("backup");
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
        let Commands::Restore(args) = cli.command else {
            panic!("restore command was not parsed");
        };
        assert_eq!(args.paths, ["-literal"]);
    }

    #[test]
    fn backup_run_subcommand_is_optional() {
        let short = Cli::try_parse_from(["boxup", "backup"]).unwrap();
        let Commands::Backup { command } = short.command else {
            panic!("backup command was not parsed");
        };
        assert!(command.is_none());

        let legacy = Cli::try_parse_from(["boxup", "backup", "run"]).unwrap();
        let Commands::Backup { command } = legacy.command else {
            panic!("backup run command was not parsed");
        };
        assert!(matches!(command, Some(BackupCommand::Run)));
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
