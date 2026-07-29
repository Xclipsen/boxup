use std::fs;
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail, ensure};
use boxup::backend::Backend;
use boxup::config::ScheduleMode;
use boxup::config::validate_id;
use boxup::domain::{BackupProtocolEvent, BackupProtocolEventKind};
use boxup::index::Index;
use boxup::jobs::{JobRunner, LocalLock, LockMode};
use boxup::{BorgBackend, Config};
use clap::{Parser, Subcommand};
use futures::TryStreamExt;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "boxup-root",
    version,
    about = "Fixed privileged operations for Boxup"
)]
struct Cli {
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Subcommand)]
enum Operation {
    Init,
    Backup {
        #[arg(long)]
        progress_json: bool,
    },
    Due,
    Maintenance {
        #[arg(long)]
        dry_run: bool,
    },
    Check {
        #[arg(long)]
        verify_data: bool,
    },
    IndexRefresh,
    KeyExport,
    Prepare,
    ValidateConfig,
    PrintSchedule,
    Snapshots {
        #[arg(long)]
        json: bool,
    },
    Ls {
        snapshot: String,
        path: Option<String>,
        #[arg(long)]
        json: bool,
    },
    RestoreOverwrite {
        snapshot: String,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    RestoreOriginal {
        #[arg(long)]
        confirm: String,
        snapshot: String,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    RestoreSafe {
        #[arg(long, value_name = "PATH")]
        to: PathBuf,
        snapshot: String,
        #[arg(required = true)]
        paths: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    if let Err(error) = run().await {
        tracing::error!("{error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    ensure!(
        nix::unistd::Uid::effective().is_root(),
        "boxup-root must run as root"
    );
    let config_path = validate_root_config(&cli.config)?;
    let config = Config::load(&config_path)?;
    config.validate_system_profile(&config_path)?;
    validate_system_credentials(&config)?;
    if matches!(&cli.operation, Operation::Init) {
        ensure_new_setup_mode(&config)?;
    }
    if matches!(
        &cli.operation,
        Operation::Init
            | Operation::Backup { .. }
            | Operation::Due
            | Operation::Maintenance { .. }
            | Operation::Check { .. }
            | Operation::IndexRefresh
    ) {
        ensure_live_validation_completed(&config)?;
    }
    match &cli.operation {
        Operation::ValidateConfig => return Ok(()),
        Operation::PrintSchedule => {
            match config.schedule.mode {
                ScheduleMode::Due => println!("due"),
                ScheduleMode::Calendar => println!(
                    "calendar\t{}",
                    config
                        .schedule
                        .calendar
                        .as_deref()
                        .context("calendar schedule is missing")?
                ),
            }
            return Ok(());
        }
        Operation::Prepare => {
            Index::open(&config.index.path)?;
            return Ok(());
        }
        _ => {}
    }
    if let Operation::RestoreSafe { to, .. } = &cli.operation {
        boxup::restore::prepare_safe_restore_target(&config, to)?;
    }
    let mut cancellation = if matches!(
        &cli.operation,
        Operation::Backup {
            progress_json: true
        }
    ) {
        Some(interactive_cancellation()?)
    } else {
        None
    };
    let backend = BorgBackend::new(&config);
    if let Some((_, cancel)) = &mut cancellation {
        cancellable_preflight(&backend, cancel).await?;
    } else {
        backend.preflight().await?;
    }
    match cli.operation {
        Operation::Init => {
            let _lock = LocalLock::acquire(&config.backup.state_dir, LockMode::Exclusive)?;
            backend.init_repository().await?;
            complete_live_validation(&config)?;
        }
        Operation::Backup { progress_json } => {
            let index = Index::open(&config.index.path)?;
            let runner = JobRunner::new(&config, &backend, &index);
            if progress_json {
                let output = Mutex::new(BufWriter::new(std::io::stdout()));
                let (cancel_sender, cancel) = cancellation
                    .take()
                    .context("interactive backup cancellation was not initialized")?;
                let snapshot = runner
                    .backup_with_progress_cancellable(
                        |progress| {
                            if write_backup_event(
                                &output,
                                &BackupProtocolEvent {
                                    version: 1,
                                    event: BackupProtocolEventKind::Progress { progress },
                                },
                            )
                            .is_err()
                            {
                                let _ = cancel_sender.send(true);
                            }
                        },
                        cancel,
                    )
                    .await?;
                write_backup_event(
                    &output,
                    &BackupProtocolEvent {
                        version: 1,
                        event: BackupProtocolEventKind::Result { snapshot },
                    },
                )?;
            } else {
                runner.backup().await?;
            }
        }
        Operation::Due => {
            let index = Index::open(&config.index.path)?;
            JobRunner::new(&config, &backend, &index)
                .backup_if_due()
                .await?;
        }
        Operation::Maintenance { dry_run } => {
            let index = Index::open(&config.index.path)?;
            JobRunner::new(&config, &backend, &index)
                .prune(dry_run)
                .await?;
        }
        Operation::Check { verify_data } => {
            let index = Index::open(&config.index.path)?;
            JobRunner::new(&config, &backend, &index)
                .check(verify_data)
                .await?;
        }
        Operation::IndexRefresh => {
            if LocalLock::is_held(&config.backup.state_dir)? {
                tracing::info!("index refresh skipped because another Boxup operation is active");
            } else {
                let index = Index::open(&config.index.path)?;
                JobRunner::new(&config, &backend, &index)
                    .refresh_index()
                    .await?;
            }
        }
        Operation::KeyExport => {
            let destination = Path::new("/etc/boxup").join(format!("{}.repokey", config.host.id));
            backend.key_export(&destination).await?;
            println!("Exported repository key to {}", destination.display());
        }
        Operation::Snapshots { json } => {
            let snapshots = backend.list_snapshots().await?;
            ensure!(
                snapshots
                    .iter()
                    .any(|snapshot| snapshot.name.starts_with(&config.archive_prefix())),
                "live repository has no archive matching this profile"
            );
            complete_live_validation(&config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else {
                for snapshot in snapshots {
                    println!("{}\t{}", snapshot.start.to_rfc3339(), snapshot.name);
                }
            }
        }
        Operation::Ls {
            snapshot,
            path,
            json,
        } => {
            let mut stream = backend.list_files(&snapshot, path.as_deref()).await?;
            let mut first = true;
            if json {
                print!("[");
            }
            while let Some(item) = stream.try_next().await? {
                if json {
                    if !first {
                        print!(",");
                    }
                    print!("{}", serde_json::to_string(&item)?);
                    first = false;
                } else {
                    println!("{:?}\t{}\t{}", item.kind, item.size, item.path);
                }
            }
            if json {
                println!("]");
            }
        }
        Operation::RestoreOverwrite { snapshot, paths } => {
            boxup::restore::restore_overwrite_root(&backend, &config, &snapshot, &paths).await?;
        }
        Operation::RestoreOriginal {
            confirm,
            snapshot,
            paths,
        } => {
            ensure!(
                confirm == "RESTORE",
                "original-path restore confirmation did not match"
            );
            protect_operation_from_terminal_signals()?;
            let output = Mutex::new(BufWriter::new(std::io::stdout()));
            boxup::restore::restore_original_root(&backend, &config, &snapshot, &paths, |event| {
                let _ = write_json_line(&output, &event);
            })
            .await?;
        }
        Operation::RestoreSafe {
            to,
            snapshot,
            paths,
        } => {
            let plan = boxup::restore::restore(&backend, &config, &snapshot, &paths, &to).await?;
            println!(
                "{}",
                serde_json::json!({
                    "destination": to,
                    "files": plan.files,
                    "bytes": plan.bytes,
                })
            );
        }
        Operation::Prepare | Operation::ValidateConfig | Operation::PrintSchedule => unreachable!(),
    }
    Ok(())
}

fn write_backup_event(
    output: &Mutex<BufWriter<std::io::Stdout>>,
    event: &BackupProtocolEvent,
) -> Result<()> {
    write_json_line(output, event)
}

fn ensure_live_validation_completed(config: &Config) -> Result<()> {
    let marker = validation_marker(config);
    match fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == 0
                    && metadata.mode() & 0o022 == 0,
                "live validation marker is unsafe"
            );
            bail!("run 'boxup snapshots --live' before writing to this repository")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn complete_live_validation(config: &Config) -> Result<()> {
    let marker = validation_marker(config);
    match fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == 0
                    && metadata.mode() & 0o022 == 0,
                "live validation marker is unsafe"
            );
            fs::remove_file(marker)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validation_marker(config: &Config) -> PathBuf {
    config.backup.state_dir.join("requires-live-validation")
}

fn ensure_new_setup_mode(config: &Config) -> Result<()> {
    let path = config.backup.state_dir.join("setup-mode");
    let metadata = fs::symlink_metadata(&path).context("setup mode is unavailable")?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 32,
        "setup mode file is unsafe"
    );
    ensure!(
        fs::read_to_string(path)?.trim() == "new",
        "repository initialization is available only for profiles installed explicitly in new mode"
    );
    Ok(())
}

fn write_json_line<T: serde::Serialize>(
    output: &Mutex<BufWriter<std::io::Stdout>>,
    event: &T,
) -> Result<()> {
    let mut output = output
        .lock()
        .map_err(|_| anyhow!("backup progress output lock was poisoned"))?;
    serde_json::to_writer(&mut *output, event)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

fn protect_operation_from_terminal_signals() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    for kind in [
        SignalKind::interrupt(),
        SignalKind::terminate(),
        SignalKind::hangup(),
    ] {
        let mut signals = signal(kind)?;
        tokio::spawn(async move { while signals.recv().await.is_some() {} });
    }
    Ok(())
}

fn interactive_cancellation() -> Result<(
    tokio::sync::watch::Sender<bool>,
    tokio::sync::watch::Receiver<bool>,
)> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let (sender, receiver) = tokio::sync::watch::channel(false);
    let signal_sender = sender.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
        let _ = signal_sender.send(true);
    });
    let stdin_sender = sender.clone();
    std::thread::spawn(move || {
        let mut byte = [0_u8; 1];
        let _ = std::io::stdin().read(&mut byte);
        let _ = stdin_sender.send(true);
    });
    Ok((sender, receiver))
}

async fn cancellable_preflight(
    backend: &BorgBackend,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    ensure!(!*cancel.borrow(), "backup cancelled before Borg preflight");
    let preflight = backend.preflight();
    tokio::pin!(preflight);
    loop {
        tokio::select! {
            result = &mut preflight => return result,
            changed = cancel.changed() => {
                if changed.is_ok() && *cancel.borrow_and_update() {
                    bail!("backup cancelled during Borg preflight");
                }
            }
        }
    }
}

fn validate_root_config(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "config path must be absolute");
    ensure!(
        path.extension()
            .is_some_and(|extension| extension == "toml"),
        "config must have a .toml extension"
    );
    let path_metadata = path
        .symlink_metadata()
        .context("failed to inspect config")?;
    ensure!(
        path_metadata.is_file() && !path_metadata.file_type().is_symlink(),
        "root helper config must be a regular non-symlink file"
    );
    let canonical = path
        .canonicalize()
        .context("failed to canonicalize config")?;
    let root = Path::new("/etc/boxup")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("/etc/boxup"));
    ensure!(
        canonical.parent() == Some(root.as_path()),
        "root helper accepts only /etc/boxup/*.toml profiles"
    );
    let profile = canonical
        .file_stem()
        .and_then(|value| value.to_str())
        .context("config profile name is not valid UTF-8")?;
    validate_id("config profile", profile)?;
    let root_metadata = root.metadata()?;
    ensure!(
        root_metadata.is_dir() && root_metadata.uid() == 0 && root_metadata.mode() & 0o022 == 0,
        "/etc/boxup must be a root-owned non-writable directory"
    );
    let metadata = canonical.metadata()?;
    ensure!(
        metadata.uid() == 0,
        "root helper config must be owned by root"
    );
    ensure!(
        metadata.mode() & 0o077 == 0,
        "root helper config must be accessible only by root"
    );
    Ok(canonical)
}

fn validate_system_credentials(config: &Config) -> Result<()> {
    let mut secrets = vec![
        config.repository.passphrase_file.as_path(),
        config.repository.ssh_key.as_path(),
    ];
    if let Some(path) = &config.repository.maintenance_ssh_key {
        secrets.push(path);
    }
    if config.notifications.enabled {
        if let Some(path) = &config.notifications.discord_webhook_file {
            secrets.push(path);
        }
    }
    for path in secrets {
        let metadata = path
            .symlink_metadata()
            .with_context(|| format!("failed to inspect credential {}", path.display()))?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == 0
                && metadata.mode() & 0o077 == 0,
            "credential must be a root-owned mode-0600-or-tighter regular file: {}",
            path.display()
        );
    }
    let known_hosts = config.repository.known_hosts.symlink_metadata()?;
    ensure!(
        known_hosts.is_file()
            && !known_hosts.file_type().is_symlink()
            && known_hosts.uid() == 0
            && known_hosts.mode() & 0o022 == 0,
        "known_hosts must be a root-owned non-writable regular file"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_restore_accepts_literal_option_shaped_paths() {
        let cli = Cli::try_parse_from([
            "boxup-root",
            "--config",
            "/etc/boxup/desktop.toml",
            "restore-original",
            "--confirm",
            "RESTORE",
            "desktop-archive",
            "--",
            "-literal",
        ])
        .unwrap();
        let Operation::RestoreOriginal {
            confirm,
            snapshot,
            paths,
        } = cli.operation
        else {
            panic!("restore-original was not parsed");
        };
        assert_eq!(confirm, "RESTORE");
        assert_eq!(snapshot, "desktop-archive");
        assert_eq!(paths, ["-literal"]);
    }

    #[test]
    fn safe_restore_parses_fixed_destination_and_literal_paths() {
        let cli = Cli::try_parse_from([
            "boxup-root",
            "--config",
            "/etc/boxup/desktop.toml",
            "restore-safe",
            "--to",
            "/var/lib/boxup-recovery/desktop/restore-20260728T142305Z",
            "desktop-archive",
            "--",
            "-literal",
        ])
        .unwrap();
        let Operation::RestoreSafe {
            to,
            snapshot,
            paths,
        } = cli.operation
        else {
            panic!("restore-safe was not parsed");
        };
        assert_eq!(
            to,
            Path::new("/var/lib/boxup-recovery/desktop/restore-20260728T142305Z")
        );
        assert_eq!(snapshot, "desktop-archive");
        assert_eq!(paths, ["-literal"]);
    }

    #[test]
    fn backup_progress_protocol_is_explicit() {
        let cli = Cli::try_parse_from([
            "boxup-root",
            "--config",
            "/etc/boxup/desktop.toml",
            "backup",
            "--progress-json",
        ])
        .unwrap();
        assert!(matches!(
            cli.operation,
            Operation::Backup {
                progress_json: true
            }
        ));
    }
}
