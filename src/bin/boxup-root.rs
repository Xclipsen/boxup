use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use boxup::backend::Backend;
use boxup::config::ScheduleMode;
use boxup::config::validate_id;
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
    Backup,
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
    let backend = BorgBackend::new(&config);
    backend.preflight().await?;
    match cli.operation {
        Operation::Init => {
            let _lock = LocalLock::acquire(&config.backup.state_dir, LockMode::Exclusive)?;
            backend.init_repository().await?;
        }
        Operation::Backup => {
            let index = Index::open(&config.index.path)?;
            JobRunner::new(&config, &backend, &index).backup().await?;
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
            let index = Index::open(&config.index.path)?;
            JobRunner::new(&config, &backend, &index)
                .refresh_index()
                .await?;
        }
        Operation::KeyExport => {
            let destination = Path::new("/etc/boxup").join(format!("{}.repokey", config.host.id));
            backend.key_export(&destination).await?;
            println!("Exported repository key to {}", destination.display());
        }
        Operation::Snapshots { json } => {
            let snapshots = backend.list_snapshots().await?;
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
            boxup::restore::restore_original_root(&backend, &config, &snapshot, &paths, |event| {
                println!(
                    "{}",
                    serde_json::to_string(&event).expect("restore progress is serializable")
                );
                let _ = std::io::stdout().flush();
            })
            .await?;
        }
        Operation::Prepare | Operation::ValidateConfig | Operation::PrintSchedule => unreachable!(),
    }
    Ok(())
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
}
