use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use anyhow::{Context, Result, bail, ensure};
use chrono::{Duration, Utc};
use nix::sys::statvfs::statvfs;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

const DOCKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const RSYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
const PG_DUMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const CURL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const REPOSITORY_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const BACKUP_REPOSITORY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(23 * 60 * 60);
const INDEX_JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(23 * 60 * 60);
const MAINTENANCE_JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(47 * 60 * 60);
const CHECK_JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(47 * 60 * 60);

use crate::backend::Backend;
use crate::config::{Config, validate_id, validate_service_name};
use crate::domain::{CreateRequest, Snapshot, utc_now};
use crate::index::{Index, RefreshStats};

pub struct JobRunner<'a, B: Backend + ?Sized> {
    config: &'a Config,
    backend: &'a B,
    index: &'a Index,
}

impl<'a, B: Backend + ?Sized> JobRunner<'a, B> {
    pub fn new(config: &'a Config, backend: &'a B, index: &'a Index) -> Self {
        Self {
            config,
            backend,
            index,
        }
    }

    pub async fn backup(&self) -> Result<Snapshot> {
        let _lock = LocalLock::acquire(&self.config.backup.state_dir, LockMode::Exclusive)?;
        let job = self.index.start_job("backup")?;
        let result = self.backup_inner().await;
        self.record_result(job, "backup", &result).await?;
        result
    }

    pub async fn backup_if_due(&self) -> Result<Option<Snapshot>> {
        if let Some(last) = self.index.last_success("backup")? {
            if utc_now() - last < Duration::hours(self.config.schedule.due_hours as i64) {
                tracing::info!("backup is not due");
                return Ok(None);
            }
        }
        self.backup().await.map(Some)
    }

    pub async fn refresh_index(&self) -> Result<RefreshStats> {
        let _lock = LocalLock::acquire(&self.config.backup.state_dir, LockMode::Exclusive)?;
        let job = self.index.start_job("index")?;
        let result = bounded_operation(
            "index refresh",
            INDEX_JOB_TIMEOUT,
            self.index.refresh(self.backend),
        )
        .await;
        self.record_result(job, "index", &result).await?;
        result
    }

    pub async fn prune(&self, dry_run: bool) -> Result<()> {
        let _lock = LocalLock::acquire(&self.config.backup.state_dir, LockMode::Exclusive)?;
        let stamp = read_success_stamp(self.config)?;
        ensure!(
            utc_now() - stamp.completed_at
                <= Duration::hours(self.config.retention.require_backup_within_hours as i64),
            "prune refused: the last fully successful backup is too old"
        );
        ensure!(
            stamp.completed_at <= utc_now() + Duration::minutes(5),
            "prune refused: the success stamp is in the future"
        );
        let live = bounded_operation(
            "live archive validation",
            REPOSITORY_QUERY_TIMEOUT,
            self.backend.list_snapshots(),
        )
        .await?;
        ensure!(
            live.iter().any(|snapshot| {
                snapshot.name == stamp.archive
                    && snapshot.id == stamp.archive_id
                    && snapshot.name.starts_with(&self.config.archive_prefix())
            }),
            "prune refused: the stamped backup is absent from the live repository"
        );
        let job = self.index.start_job("maintenance")?;
        let result = bounded_operation("maintenance job", MAINTENANCE_JOB_TIMEOUT, async {
            self.backend
                .prune(
                    &self.config.archive_prefix(),
                    (
                        self.config.retention.keep_daily,
                        self.config.retention.keep_weekly,
                        self.config.retention.keep_monthly,
                    ),
                    dry_run,
                )
                .await?;
            if !dry_run {
                self.backend.compact().await?;
                self.index.refresh(self.backend).await?;
            }
            Ok(())
        })
        .await;
        self.record_result(job, "maintenance", &result).await?;
        result
    }

    pub async fn check(&self, verify_data: bool) -> Result<()> {
        let _lock = LocalLock::acquire(&self.config.backup.state_dir, LockMode::Exclusive)?;
        let job = self.index.start_job("check")?;
        let result = bounded_operation(
            "repository check",
            CHECK_JOB_TIMEOUT,
            self.backend.check(verify_data),
        )
        .await;
        self.record_result(job, "check", &result).await?;
        result
    }

    async fn backup_inner(&self) -> Result<Snapshot> {
        create_private_dir(&self.config.backup.state_dir)?;
        create_private_dir(&self.config.backup.cache_dir)?;
        let docker = DockerManager::new(self.config);
        docker.recover_unfinished().await?;
        let audit = docker.audit(true).await?;
        let docker_snapshot = docker.prepare_snapshot(&audit).await?;

        let archive_name = format!(
            "{}-{}-{}",
            self.config.host.id,
            utc_now().format("%Y%m%dT%H%M%S%fZ"),
            std::process::id()
        );
        let inventory_dir = self.config.backup.state_dir.join("inventory");
        create_private_dir(&inventory_dir)?;
        let inventory = inventory_dir.join(format!("{archive_name}.json"));
        let inventory_value = serde_json::json!({
            "archive": archive_name,
            "created_at": utc_now(),
            "sources": self.config.backup.sources,
            "excludes": self.config.backup.excludes,
            "docker": audit,
            "docker_staged_sources": docker_snapshot.as_ref().map(|snapshot| &snapshot.staged_sources),
        });
        write_atomic(&inventory, &serde_json::to_vec_pretty(&inventory_value)?)?;

        let mut sources = self.config.backup.sources.clone();
        add_source_if_uncovered(&mut sources, inventory, self.config.backup.one_file_system);
        if let Some(snapshot) = &docker_snapshot {
            add_source_if_uncovered(
                &mut sources,
                snapshot.source.clone(),
                self.config.backup.one_file_system,
            );
        }
        let mut excludes = self.config.backup.excludes.clone();
        for (path, prefix) in [
            (&self.config.backup.cache_dir, true),
            (&self.config.restore.staging_dir, true),
            (&self.config.index.path, false),
            (&self.config.backup.state_dir.join("boxup.lock"), false),
            (
                &self.config.backup.state_dir.join("last-success.json"),
                false,
            ),
        ] {
            add_internal_exclude(&mut excludes, path, prefix)?;
        }
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = self.config.index.path.as_os_str().to_os_string();
            sidecar.push(suffix);
            add_internal_exclude(&mut excludes, Path::new(&sidecar), false)?;
        }
        if let Some(snapshot) = &docker_snapshot {
            for source in &snapshot.staged_sources {
                add_internal_exclude(&mut excludes, source, true)?;
            }
        }
        let request = CreateRequest {
            archive_name,
            sources,
            excludes,
            one_file_system: self.config.backup.one_file_system,
            exclude_caches: self.config.backup.exclude_caches,
            compression: self.config.backup.compression.clone(),
            upload_rate_kib: self.config.backup.upload_rate_kib,
        };
        // Containers have been resumed before this deadline starts, so cancellation cannot
        // strand a quiesced workload.
        let snapshot = bounded_operation(
            "backup repository phase",
            BACKUP_REPOSITORY_TIMEOUT,
            async {
                let snapshot = self.backend.create(&request).await?;
                ensure!(
                    snapshot.name == request.archive_name,
                    "backend created an unexpected archive name"
                );
                validate_archive_id("created archive id", &snapshot.id)?;
                self.index.refresh(self.backend).await?;
                Result::<Snapshot>::Ok(snapshot)
            },
        )
        .await?;
        write_atomic(
            &self.config.backup.state_dir.join("last-success.json"),
            &serde_json::to_vec_pretty(&SuccessStamp {
                version: 2,
                host: self.config.host.id.clone(),
                archive: snapshot.name.clone(),
                archive_id: snapshot.id.clone(),
                completed_at: utc_now(),
            })?,
        )?;
        Ok(snapshot)
    }

    async fn record_result<T>(&self, job: i64, kind: &str, result: &Result<T>) -> Result<()> {
        let (success, message) = match result {
            Ok(_) => (true, None),
            Err(_) => (false, Some(format!("{kind} failed; see privileged logs"))),
        };
        self.index.finish_job(job, success, message.as_deref())?;
        if let Err(error) = notify(self.config, kind, success).await {
            tracing::warn!("notification failed: {error:#}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LockMode {
    Shared,
    Exclusive,
}

pub struct LocalLock {
    file: File,
}

impl LocalLock {
    pub fn acquire(state_dir: &Path, mode: LockMode) -> Result<Self> {
        create_private_dir(state_dir)?;
        let path = state_dir.join("boxup.lock");
        let file = match mode {
            LockMode::Shared => OpenOptions::new().read(true).open(&path).or_else(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(&path)
                } else {
                    Err(error)
                }
            })?,
            LockMode::Exclusive => OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)?,
        };
        let operation = match mode {
            LockMode::Shared => libc::LOCK_SH,
            LockMode::Exclusive => libc::LOCK_EX,
        } | libc::LOCK_NB;
        // SAFETY: flock only reads the valid open descriptor and does not retain the pointer state.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("another Boxup operation holds {}", path.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for LocalLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until after Drop returns.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DockerAudit {
    pub available: bool,
    pub containers: Vec<ContainerAudit>,
    pub compose_projects: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerAudit {
    pub id: String,
    pub name: String,
    pub image: String,
    pub running: bool,
    pub stateful: bool,
    pub postgres: bool,
    pub compose_project: Option<String>,
    pub mounts: Vec<MountAudit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MountAudit {
    pub kind: String,
    pub source: PathBuf,
    pub destination: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DockerSnapshot {
    pub source: PathBuf,
    pub staged_sources: Vec<PathBuf>,
}

pub struct DockerManager<'a> {
    config: &'a Config,
}

impl<'a> DockerManager<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    pub async fn audit(&self, all: bool) -> Result<DockerAudit> {
        if !self.config.docker.enabled {
            return Ok(DockerAudit {
                available: false,
                containers: Vec::new(),
                compose_projects: Vec::new(),
            });
        }
        let mut list = Command::new(&self.config.docker.docker_path);
        list.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("ps")
            .arg("-q");
        if all {
            list.arg("--all");
        }
        let output = checked_output(list, false, DOCKER_TIMEOUT).await?;
        let ids: Vec<_> = String::from_utf8(output)?
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if ids.is_empty() {
            return Ok(DockerAudit {
                available: true,
                containers: Vec::new(),
                compose_projects: Vec::new(),
            });
        }
        let mut inspect = Command::new(&self.config.docker.docker_path);
        inspect
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("inspect")
            .args(&ids);
        let raw: Vec<DockerInspect> =
            serde_json::from_slice(&checked_output(inspect, false, DOCKER_TIMEOUT).await?)?;
        ensure!(raw.len() <= 100_000, "Docker returned too many containers");
        for container in &raw {
            validate_id("Docker container id", &container.id)?;
        }
        let mut projects = HashSet::new();
        let containers = raw
            .into_iter()
            .map(|container| {
                let project = container
                    .config
                    .labels
                    .get("com.docker.compose.project")
                    .cloned();
                if let Some(value) = &project {
                    projects.insert(value.clone());
                }
                let mounts: Vec<_> = container
                    .mounts
                    .into_iter()
                    .filter(|mount| !mount.source.as_os_str().is_empty())
                    .map(|mount| MountAudit {
                        kind: mount.kind,
                        source: mount.source,
                        destination: mount.destination,
                        name: mount.name,
                    })
                    .collect();
                ContainerAudit {
                    id: container.id,
                    name: container.name.trim_start_matches('/').to_owned(),
                    postgres: is_official_postgres(&container.config.image),
                    image: container.config.image,
                    running: container.state.running,
                    stateful: mounts.iter().any(|mount| self.mount_is_stageable(mount)),
                    compose_project: project,
                    mounts,
                }
            })
            .collect();
        let mut compose_projects: Vec<_> = projects.into_iter().collect();
        compose_projects.sort();
        Ok(DockerAudit {
            available: true,
            containers,
            compose_projects,
        })
    }

    pub async fn recover_unfinished(&self) -> Result<()> {
        if !self.config.docker.enabled {
            return Ok(());
        }
        let journal_path = self.journal_path()?;
        if !journal_path.exists() {
            return Ok(());
        }
        let journal_metadata = fs::symlink_metadata(&journal_path)?;
        ensure!(
            journal_metadata.is_file()
                && !journal_metadata.file_type().is_symlink()
                && journal_metadata.len() <= 1024 * 1024,
            "Docker recovery journal is not a bounded regular file"
        );
        let journal: QuiesceJournal = serde_json::from_slice(&fs::read(&journal_path)?)?;
        ensure!(
            journal.active.len() <= 10_000,
            "Docker recovery journal is unreasonably large"
        );
        ensure!(
            journal.services.len() <= 10_000,
            "service recovery journal is unreasonably large"
        );
        ensure!(
            matches!(journal.phase.as_str(), "stopping" | "quiesced"),
            "Docker recovery journal has an invalid phase"
        );
        let mut unique = HashSet::new();
        for id in &journal.active {
            validate_id("Docker recovery container id", id)?;
            ensure!(
                unique.insert(id),
                "Docker recovery journal repeats a container id"
            );
        }
        unique.clear();
        for service in &journal.services {
            validate_service_name(service)?;
            ensure!(
                unique.insert(service),
                "Docker recovery journal repeats a service name"
            );
        }
        self.resume(&journal.active, &journal.services).await?;
        fs::remove_file(journal_path)?;
        Ok(())
    }

    pub async fn prepare_snapshot(&self, audit: &DockerAudit) -> Result<Option<DockerSnapshot>> {
        if !self.config.docker.enabled {
            return Ok(None);
        }
        let staging = self
            .config
            .docker
            .staging_dir
            .as_ref()
            .context("Docker staging is not configured")?;
        create_private_dir(staging)?;
        let stats = statvfs(staging)?;
        let free = stats
            .blocks_available()
            .saturating_mul(stats.fragment_size());
        ensure!(
            free >= self.config.docker.min_free_bytes,
            "Docker snapshot refused: staging free space is below min_free_bytes"
        );

        let selected: Vec<_> = audit
            .containers
            .iter()
            .filter(|container| container.stateful)
            .filter(|container| {
                self.config.docker.stop_all_stateful
                    || self
                        .config
                        .docker
                        .stop_containers
                        .iter()
                        .any(|value| value == &container.id || value == &container.name)
            })
            .collect();
        clean_stale_staging(
            staging,
            &selected,
            self.config.docker.service_paths.len(),
            |mount| self.mount_is_stageable(mount),
        )?;
        if selected.is_empty() && self.config.docker.service_paths.is_empty() {
            return Ok(None);
        }

        let mut staged_sources = Vec::new();
        let active: Vec<_> = selected
            .iter()
            .filter(|container| container.running)
            .copied()
            .collect();
        for container in selected.iter().filter(|container| !container.running) {
            ensure!(
                !self.container_is_running(&container.id).await?,
                "stopped container {} became active after the audit; refusing to stage it without quiescing",
                container.name
            );
            staged_sources.extend(self.copy_mounts(container, staging, false).await?);
        }
        for container in &active {
            self.copy_mounts(container, staging, false).await?;
            if container.postgres {
                self.dump_postgres(container, staging).await?;
            }
        }
        let staged_services = self.copy_service_paths(staging, false).await?;

        let mut active_services = Vec::new();
        for service in &self.config.docker.stop_services {
            if self.service_is_active(service).await? {
                active_services.push(service.clone());
            }
        }

        let active: Vec<_> = selected
            .iter()
            .filter(|container| container.running)
            .map(|container| container.id.clone())
            .collect();
        if active.is_empty() && active_services.is_empty() {
            staged_sources.extend(staged_services);
            staged_sources.sort();
            staged_sources.dedup();
            return Ok(Some(DockerSnapshot {
                source: staging.clone(),
                staged_sources,
            }));
        }
        let journal_path = self.journal_path()?;
        write_atomic(
            &journal_path,
            &serde_json::to_vec_pretty(&QuiesceJournal {
                active: active.clone(),
                services: active_services.clone(),
                phase: "stopping".into(),
                created_at: utc_now(),
            })?,
        )?;
        let quiesce_result: Result<Vec<PathBuf>> = async {
            for service in &active_services {
                self.service_action("stop", service, true).await?;
            }
            for id in &active {
                self.docker_action("stop", id, true).await?;
            }
            write_atomic(
                &journal_path,
                &serde_json::to_vec_pretty(&QuiesceJournal {
                    active: active.clone(),
                    services: active_services.clone(),
                    phase: "quiesced".into(),
                    created_at: utc_now(),
                })?,
            )?;
            let mut completed = staged_sources;
            for container in selected
                .iter()
                .filter(|container| container.running || !active_services.is_empty())
            {
                completed.extend(self.copy_mounts(container, staging, true).await?);
            }
            completed.extend(self.copy_service_paths(staging, true).await?);
            completed.sort();
            completed.dedup();
            Ok(completed)
        }
        .await;
        let resume_result = self.resume(&active, &active_services).await;
        match (quiesce_result, resume_result) {
            (Ok(staged_sources), Ok(())) => {
                fs::remove_file(&journal_path)?;
                Ok(Some(DockerSnapshot {
                    source: staging.clone(),
                    staged_sources,
                }))
            }
            (Err(snapshot_error), Ok(())) => {
                fs::remove_file(&journal_path)?;
                Err(snapshot_error)
            }
            (Ok(_), Err(restart_error)) => Err(restart_error.context(
                "Docker staging completed, but container restart/verification failed; recovery journal retained",
            )),
            (Err(snapshot_error), Err(restart_error)) => Err(restart_error.context(format!(
                "container restart/verification failed after snapshot failure ({snapshot_error:#}); recovery journal retained"
            ))),
        }
    }

    async fn copy_mounts(
        &self,
        container: &ContainerAudit,
        staging: &Path,
        interruptible: bool,
    ) -> Result<Vec<PathBuf>> {
        let mut staged = Vec::new();
        for (position, mount) in container.mounts.iter().enumerate() {
            if !self.mount_is_stageable(mount) {
                continue;
            }
            ensure!(
                mount.source.is_absolute(),
                "Docker mount source is not absolute"
            );
            let destination = staging
                .join("mounts")
                .join(&container.id)
                .join(position.to_string());
            self.copy_path(&mount.source, &destination, interruptible)
                .await?;
            staged.push(mount.source.clone());
        }
        Ok(staged)
    }

    async fn copy_service_paths(
        &self,
        staging: &Path,
        interruptible: bool,
    ) -> Result<Vec<PathBuf>> {
        let mut staged = Vec::new();
        for (position, path) in self.config.docker.service_paths.iter().enumerate() {
            let destination = staging.join("services").join(position.to_string());
            self.copy_path(path, &destination, interruptible).await?;
            staged.push(path.clone());
        }
        Ok(staged)
    }

    async fn copy_path(
        &self,
        source: &Path,
        destination: &Path,
        interruptible: bool,
    ) -> Result<()> {
        let metadata = fs::symlink_metadata(source)
            .with_context(|| format!("failed to inspect staged path {}", source.display()))?;
        ensure!(
            metadata.is_dir() || metadata.is_file() || metadata.file_type().is_symlink(),
            "staged path is not a regular file, directory, or symlink: {}",
            source.display()
        );
        create_private_dir(destination)?;
        let mut source_argument = source.as_os_str().to_os_string();
        if metadata.is_dir() {
            source_argument.push("/");
        }
        for attempt in 1..=5 {
            let mut command = Command::new(&self.config.docker.rsync_path);
            command
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .args(["-aHAXS", "--delete", "--numeric-ids", "--"])
                .arg(&source_argument)
                .arg(destination);
            let (status, _) = run_command(command, None, false, interruptible, RSYNC_TIMEOUT)
                .await
                .with_context(|| format!("rsync failed for staged path {}", source.display()))?;
            if status.success() {
                return Ok(());
            }
            if !interruptible && matches!(status.code(), Some(23) | Some(24)) && attempt < 5 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
            bail!(
                "rsync failed for staged path {} with {status}",
                source.display()
            );
        }
        unreachable!("bounded rsync retry loop always returns")
    }

    async fn dump_postgres(&self, container: &ContainerAudit, staging: &Path) -> Result<()> {
        let dumps = staging.join("postgres");
        create_private_dir(&dumps)?;
        let output_path = dumps.join(format!("{}.sql", container.id));
        let output_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&output_path)?;
        let role = self
            .config
            .docker
            .postgres_users
            .get(&container.id)
            .or_else(|| self.config.docker.postgres_users.get(&container.name))
            .map(String::as_str)
            .unwrap_or("postgres");
        let mut command = Command::new(&self.config.docker.docker_path);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["exec", &container.id, "pg_dumpall", "-U", role])
            .stdout(Stdio::from(output_file));
        checked_status(command, true, PG_DUMP_TIMEOUT)
            .await
            .with_context(|| format!("pg_dumpall failed for container {}", container.name))?;
        Ok(())
    }

    async fn docker_action(&self, action: &str, id: &str, interruptible: bool) -> Result<()> {
        let mut command = Command::new(&self.config.docker.docker_path);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg(action)
            .arg(id);
        checked_output(command, interruptible, DOCKER_TIMEOUT).await?;
        Ok(())
    }

    async fn service_action(&self, action: &str, service: &str, interruptible: bool) -> Result<()> {
        let mut command = Command::new(&self.config.docker.systemctl_path);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg(action)
            .arg(service);
        checked_output(command, interruptible, DOCKER_TIMEOUT).await?;
        Ok(())
    }

    async fn service_is_active(&self, service: &str) -> Result<bool> {
        let mut command = Command::new(&self.config.docker.systemctl_path);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["is-active", "--quiet", service]);
        let status = command_status(command, false, DOCKER_TIMEOUT).await?;
        match status.code() {
            Some(0) => Ok(true),
            Some(3) => Ok(false),
            _ => bail!("systemctl could not determine whether {service} is active: {status}"),
        }
    }

    async fn resume(&self, ids: &[String], services: &[String]) -> Result<()> {
        let mut failures = Vec::new();
        for id in ids {
            let start = self.docker_action("start", id, false).await;
            match (start, self.container_is_running(id).await) {
                (_, Ok(true)) => {}
                (Ok(()), Ok(false)) => {
                    failures.push(format!("{id}: Docker reports container is stopped"))
                }
                (Err(start_error), Ok(false)) => failures.push(format!(
                    "{id}: start failed: {start_error:#}; Docker reports container is stopped"
                )),
                (Ok(()), Err(check_error)) => failures.push(format!(
                    "{id}: running-state check failed: {check_error:#}"
                )),
                (Err(start_error), Err(check_error)) => failures.push(format!(
                    "{id}: start failed: {start_error:#}; running-state check failed: {check_error:#}"
                )),
            }
        }
        for service in services {
            let start = self.service_action("start", service, false).await;
            match (start, self.service_is_active(service).await) {
                (_, Ok(true)) => {}
                (Ok(()), Ok(false)) => {
                    failures.push(format!("{service}: systemd reports service is inactive"))
                }
                (Err(start_error), Ok(false)) => failures.push(format!(
                    "{service}: start failed: {start_error:#}; systemd reports service is inactive"
                )),
                (Ok(()), Err(check_error)) => failures.push(format!(
                    "{service}: active-state check failed: {check_error:#}"
                )),
                (Err(start_error), Err(check_error)) => failures.push(format!(
                    "{service}: start failed: {start_error:#}; active-state check failed: {check_error:#}"
                )),
            }
        }
        ensure!(
            failures.is_empty(),
            "failed to restart containers/services: {}",
            failures.join(", ")
        );
        Ok(())
    }

    fn mount_is_stageable(&self, mount: &MountAudit) -> bool {
        self.config.docker.stage_mounts.is_empty()
            || self
                .config
                .docker
                .stage_mounts
                .iter()
                .any(|path| path == &mount.source)
    }

    async fn container_is_running(&self, id: &str) -> Result<bool> {
        let mut command = Command::new(&self.config.docker.docker_path);
        command.env_clear().env("PATH", "/usr/bin:/bin").args([
            "inspect",
            "--format={{.State.Running}}",
            id,
        ]);
        let output = checked_output(command, false, DOCKER_TIMEOUT).await?;
        match String::from_utf8(output)?.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => bail!("Docker returned an invalid running state"),
        }
    }

    fn journal_path(&self) -> Result<PathBuf> {
        Ok(self
            .config
            .docker
            .staging_dir
            .as_ref()
            .context("Docker staging is not configured")?
            .join("quiesce-journal.json"))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerInspect {
    id: String,
    name: String,
    config: DockerInspectConfig,
    state: DockerInspectState,
    mounts: Vec<DockerInspectMount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerInspectConfig {
    image: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerInspectState {
    running: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerInspectMount {
    #[serde(rename = "Type")]
    kind: String,
    source: PathBuf,
    destination: String,
    name: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct QuiesceJournal {
    active: Vec<String>,
    #[serde(default)]
    services: Vec<String>,
    phase: String,
    created_at: chrono::DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuccessStamp {
    version: u32,
    host: String,
    archive: String,
    archive_id: String,
    completed_at: chrono::DateTime<Utc>,
}

fn read_success_stamp(config: &Config) -> Result<SuccessStamp> {
    let path = config.backup.state_dir.join("last-success.json");
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("prune refused: no success stamp at {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "prune refused: success stamp is not a regular file"
    );
    ensure!(metadata.len() <= 64 * 1024, "success stamp is too large");
    let stamp: SuccessStamp = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(stamp.version == 2, "unsupported success stamp version");
    ensure!(stamp.host == config.host.id, "success stamp host mismatch");
    validate_id("success stamp archive", &stamp.archive)?;
    validate_archive_id("success stamp archive id", &stamp.archive_id)?;
    ensure!(
        stamp.archive.starts_with(&config.archive_prefix()),
        "success stamp archive is outside the host prefix"
    );
    Ok(stamp)
}

fn validate_archive_id(label: &str, value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid {label}"
    );
    Ok(())
}

fn is_official_postgres(image: &str) -> bool {
    let without_digest = image.split('@').next().unwrap_or(image);
    let without_tag = without_digest
        .rsplit_once(':')
        .map_or(without_digest, |(name, _)| name);
    matches!(
        without_tag,
        "postgres" | "library/postgres" | "docker.io/library/postgres"
    )
}

async fn checked_output(
    command: Command,
    interruptible: bool,
    timeout: std::time::Duration,
) -> Result<Vec<u8>> {
    let (status, output) = run_command(command, None, true, interruptible, timeout).await?;
    ensure!(status.success(), "external program failed with {status}");
    Ok(output)
}

async fn checked_status(
    command: Command,
    interruptible: bool,
    timeout: std::time::Duration,
) -> Result<()> {
    let (status, _) = run_command(command, None, false, interruptible, timeout).await?;
    ensure!(status.success(), "external program failed with {status}");
    Ok(())
}

async fn command_status(
    command: Command,
    interruptible: bool,
    timeout: std::time::Duration,
) -> Result<ExitStatus> {
    Ok(run_command(command, None, false, interruptible, timeout)
        .await?
        .0)
}

enum CommandWait<T> {
    Completed(T),
    Interrupted,
    Terminated,
    TimedOut,
}

async fn run_command(
    mut command: Command,
    input: Option<Vec<u8>>,
    capture_stdout: bool,
    interruptible: bool,
    timeout: std::time::Duration,
) -> Result<(ExitStatus, Vec<u8>)> {
    if capture_stdout {
        command.stdout(Stdio::piped());
    }
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stderr(Stdio::null()).kill_on_drop(true);

    let mut terminate_signal = if interruptible {
        Some(tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )?)
    } else {
        None
    };
    let mut child = command
        .spawn()
        .context("failed to execute external program")?;
    let stdout = if capture_stdout {
        match child.stdout.take() {
            Some(stdout) => Some(stdout),
            None => {
                terminate_and_reap(&mut child).await?;
                bail!("external program stdout was unavailable");
            }
        }
    } else {
        None
    };
    let stdin = if input.is_some() {
        match child.stdin.take() {
            Some(stdin) => Some(stdin),
            None => {
                terminate_and_reap(&mut child).await?;
                bail!("external program stdin was unavailable");
            }
        }
    } else {
        None
    };

    let mut completion = Box::pin(async {
        let read_stdout = async move {
            let mut output = Vec::new();
            if let Some(mut stdout) = stdout {
                stdout.read_to_end(&mut output).await?;
            }
            std::io::Result::Ok(output)
        };
        let write_stdin = async move {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(&input).await?;
                stdin.shutdown().await?;
            }
            std::io::Result::Ok(())
        };
        let (status, output, input) = tokio::join!(child.wait(), read_stdout, write_stdin);
        input?;
        Ok::<_, std::io::Error>((status?, output?))
    });
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let outcome = if interruptible {
        tokio::select! {
            result = &mut completion => CommandWait::Completed(result),
            _ = tokio::signal::ctrl_c() => CommandWait::Interrupted,
            _ = terminate_signal.as_mut().expect("terminate signal is configured").recv() => CommandWait::Terminated,
            _ = &mut deadline => CommandWait::TimedOut,
        }
    } else {
        tokio::select! {
            result = &mut completion => CommandWait::Completed(result),
            _ = &mut deadline => CommandWait::TimedOut,
        }
    };
    drop(completion);

    let (status, output) = match outcome {
        CommandWait::Completed(result) => result?,
        CommandWait::Interrupted => {
            terminate_and_reap(&mut child)
                .await
                .context("operation interrupted; failed to terminate and reap external program")?;
            bail!("operation interrupted");
        }
        CommandWait::Terminated => {
            terminate_and_reap(&mut child)
                .await
                .context("operation terminated; failed to terminate and reap external program")?;
            bail!("operation terminated");
        }
        CommandWait::TimedOut => {
            terminate_and_reap(&mut child).await.with_context(|| {
                format!(
                    "external program timed out after {timeout:?}; failed to terminate and reap it"
                )
            })?;
            bail!("external program timed out after {timeout:?}");
        }
    };
    Ok((status, output))
}

async fn terminate_and_reap(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    if let Err(kill_error) = child.start_kill() {
        if child.try_wait()?.is_none() {
            return Err(kill_error).context("failed to terminate external program");
        }
        return Ok(());
    }
    child
        .wait()
        .await
        .context("failed to reap terminated external program")?;
    Ok(())
}

async fn bounded_operation<T, F>(
    label: &str,
    timeout: std::time::Duration,
    operation: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::time::timeout(timeout, operation)
        .await
        .with_context(|| format!("{label} timed out after {timeout:?}"))?
}

async fn notify(config: &Config, operation: &str, success: bool) -> Result<()> {
    if !config.notifications.enabled {
        return Ok(());
    }
    let webhook = config
        .notifications
        .discord_webhook_file
        .as_ref()
        .context("notification webhook file is not configured")?;
    let webhook_metadata = fs::symlink_metadata(webhook)?;
    ensure!(
        webhook_metadata.file_type().is_file() && !webhook_metadata.file_type().is_symlink(),
        "notification webhook must be a regular non-symlink file"
    );
    ensure!(
        webhook_metadata.mode() & 0o077 == 0,
        "notification webhook must not be accessible by group or other"
    );
    let webhook_url = fs::read_to_string(webhook).context("failed to read notification webhook")?;
    let webhook_url = webhook_url.trim();
    ensure!(
        webhook_url.starts_with("https://discord.com/api/webhooks/")
            || webhook_url.starts_with("https://discordapp.com/api/webhooks/"),
        "notification file does not contain a Discord HTTPS webhook"
    );
    ensure!(
        !webhook_url.chars().any(|character| {
            character.is_control() || character.is_whitespace() || matches!(character, '"' | '\\')
        }),
        "notification webhook contains unsafe characters"
    );
    let mut curl_config = tempfile::Builder::new()
        .prefix(".curl-")
        .tempfile_in(&config.backup.state_dir)?;
    writeln!(curl_config, "url = \"{webhook_url}\"")?;
    curl_config.flush()?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "content": format!("Boxup {} {} on {}", operation, if success { "succeeded" } else { "failed" }, config.host.id)
    }))?;
    let mut command = Command::new("/usr/bin/curl");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--request",
            "POST",
        ])
        .arg("--config")
        .arg(curl_config.path())
        .args([
            "--header",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (status, _) = run_command(command, Some(payload), false, false, CURL_TIMEOUT)
        .await
        .context("curl notification failed")?;
    ensure!(status.success(), "curl notification failed with {status}");
    Ok(())
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    create_private_dir(parent)?;
    let mut file = tempfile::Builder::new()
        .prefix(".boxup-")
        .tempfile_in(parent)?;
    file.write_all(content)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|error| error.error)
        .context("failed to publish atomic state file")?;
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "private path must be a non-symlink directory: {}",
                path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700).create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn add_source_if_uncovered(sources: &mut Vec<PathBuf>, item: PathBuf, one_file_system: bool) {
    let item_device = fs::metadata(&item).ok().map(|metadata| metadata.dev());
    let covered = sources.iter().any(|source| {
        if !item.starts_with(source) {
            return false;
        }
        !one_file_system
            || fs::metadata(source)
                .ok()
                .map(|metadata| metadata.dev())
                .zip(item_device)
                .is_some_and(|(source_device, item_device)| source_device == item_device)
    });
    if !covered {
        sources.push(item);
    }
}

fn add_internal_exclude(excludes: &mut Vec<String>, path: &Path, prefix: bool) -> Result<()> {
    let relative = path
        .strip_prefix("/")?
        .to_str()
        .context("internal exclusion path is not UTF-8")?;
    let pattern = format!("{}:{relative}", if prefix { "pp" } else { "pf" });
    if !excludes.contains(&pattern) {
        excludes.push(pattern);
    }
    Ok(())
}

fn clean_stale_staging<F>(
    staging: &Path,
    selected: &[&ContainerAudit],
    service_path_count: usize,
    stageable: F,
) -> Result<()>
where
    F: Fn(&MountAudit) -> bool,
{
    let selected_ids: HashSet<_> = selected
        .iter()
        .map(|container| container.id.as_str())
        .collect();
    let mounts = staging.join("mounts");
    clean_child_directories(&mounts, |name| {
        name.to_str()
            .is_some_and(|name| selected_ids.contains(name))
    })?;
    for container in selected {
        let allowed_positions: HashSet<_> = container
            .mounts
            .iter()
            .enumerate()
            .filter(|(_, mount)| stageable(mount))
            .map(|(position, _)| position.to_string())
            .collect();
        clean_child_directories(&mounts.join(&container.id), |name| {
            name.to_str()
                .is_some_and(|name| allowed_positions.contains(name))
        })?;
    }

    let services = staging.join("services");
    if service_path_count == 0 {
        remove_staging_path(&services)?;
    } else {
        clean_child_directories(&services, |name| {
            name.to_str().is_some_and(|name| {
                name.parse::<usize>().is_ok_and(|position| {
                    position < service_path_count && name == position.to_string()
                })
            })
        })?;
    }

    let postgres = staging.join("postgres");
    match fs::symlink_metadata(&postgres) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(&postgres)?;
        }
        Ok(_) => fs::remove_file(&postgres)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn clean_child_directories<F>(path: &Path, keep: F) -> Result<()>
where
    F: Fn(&std::ffi::OsStr) -> bool,
{
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    create_private_dir(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !keep(&entry.file_name()) || !metadata.is_dir() || metadata.file_type().is_symlink() {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

fn remove_staging_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)?;
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn identifies_only_official_postgres_images() {
        assert!(is_official_postgres("postgres:17"));
        assert!(is_official_postgres("docker.io/library/postgres:16"));
        assert!(!is_official_postgres("example/postgres:17"));
        assert!(!is_official_postgres("postgis/postgis:17"));
    }

    #[test]
    fn exclusive_lock_refuses_second_writer() {
        let temp = tempfile::tempdir().unwrap();
        let _first = LocalLock::acquire(temp.path(), LockMode::Exclusive).unwrap();
        assert!(LocalLock::acquire(temp.path(), LockMode::Exclusive).is_err());
    }

    #[test]
    fn internal_excludes_are_literal_borg_patterns() {
        let mut excludes = Vec::new();
        add_internal_exclude(
            &mut excludes,
            Path::new("/var/lib/boxup/index.sqlite3"),
            false,
        )
        .unwrap();
        add_internal_exclude(&mut excludes, Path::new("/var/cache/boxup"), true).unwrap();
        assert_eq!(
            excludes,
            ["pf:var/lib/boxup/index.sqlite3", "pp:var/cache/boxup"]
        );
    }

    #[tokio::test]
    async fn timed_out_external_process_is_terminated_and_reaped() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("blocking-command");
        let pid_path = temp.path().join("pid");
        fs::write(
            &script,
            "#!/bin/sh\nset -eu\nprintf '%s' \"$$\" >\"$1\"\nexec /usr/bin/sleep 30\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).unwrap();
        let mut command = Command::new(&script);
        command.arg(&pid_path);
        let started = std::time::Instant::now();
        let error = checked_output(command, false, std::time::Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        let pid: libc::pid_t = fs::read_to_string(pid_path).unwrap().parse().unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        let mut status = 0;
        // SAFETY: waitpid only writes to the valid status pointer; the PID came from our child.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        assert_eq!(waited, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn internal_operations_have_an_enforced_timeout() {
        let error = bounded_operation(
            "test operation",
            std::time::Duration::from_millis(20),
            std::future::pending::<Result<()>>(),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("test operation timed out"));
    }
}
