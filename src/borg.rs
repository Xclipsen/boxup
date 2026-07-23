use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use futures::{Stream, TryStreamExt};
use nix::fcntl::{OFlag, RenameFlags, open, renameat2};
use nix::sys::stat::{Mode, SFlag, fstat};
use nix::unistd::Uid;
use serde::{Deserialize, Deserializer, de};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::backend::{Backend, DiffStream, FileStream};
use crate::config::{Config, RepositoryConfig, validate_id};
use crate::domain::{
    ArchiveItem, CreateRequest, DiffEntry, FileType, RepositoryIdentity, Snapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorgExit {
    Success,
    Warning,
}

#[derive(Debug)]
pub struct BorgOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit: BorgExit,
}

#[derive(Debug, Error)]
pub enum BorgError {
    #[error("Borg completed with warnings: {message}")]
    WarningExit { message: String },
    #[error("Borg exited with error code {code}: {message}")]
    ErrorExit { code: i32, message: String },
    #[error("Borg was terminated by a signal")]
    Signaled,
    #[error("Borg emitted invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct BorgRunner {
    repository: RepositoryConfig,
    cache_dir: PathBuf,
}

impl BorgRunner {
    pub fn new(repository: RepositoryConfig, cache_dir: PathBuf) -> Self {
        Self {
            repository,
            cache_dir,
        }
    }

    pub async fn run<I, S>(&self, args: I, cwd: Option<&Path>, admin: bool) -> Result<BorgOutput>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let (mut command, passphrase) = self.command(args, cwd, admin).await?;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = spawn_with_passphrase(command, passphrase).await?;
        let output = child
            .wait_with_output()
            .await
            .context("failed to wait for Borg")?;
        classify_output(output.status.code(), output.stdout, output.stderr)
    }

    async fn stream_json_lines<I, S, T, F>(
        &self,
        args: I,
        admin: bool,
        parse: F,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<T>> + Send>>>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
        T: Send + 'static,
        F: Fn(&str) -> Result<T> + Send + 'static,
    {
        let (mut command, passphrase) = self.command(args, None, admin).await?;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = spawn_with_passphrase(command, passphrase).await?;
        let stdout = child.stdout.take().context("Borg stdout was unavailable")?;
        let stderr = child.stderr.take().context("Borg stderr was unavailable")?;
        let (sender, receiver) = mpsc::channel(128);
        tokio::spawn(async move {
            let stderr_task = tokio::spawn(async move {
                let mut bytes = Vec::new();
                let _ = BufReader::new(stderr).read_to_end(&mut bytes).await;
                bytes
            });
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => {
                        let parsed = parse(&line);
                        if sender.send(parsed).await.is_err() {
                            let _ = child.kill().await;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender.send(Err(error.into())).await;
                        let _ = child.kill().await;
                        break;
                    }
                }
            }
            let status = child.wait().await;
            let stderr = stderr_task.await.unwrap_or_default();
            match status {
                Ok(status) if status.code() == Some(0) => {}
                Ok(status) if status.code().is_some_and(is_warning_code) => {
                    let _ = sender
                        .send(Err(BorgError::WarningExit {
                            message: safe_stderr(&stderr),
                        }
                        .into()))
                        .await;
                }
                Ok(status) => {
                    let error: anyhow::Error = match status.code() {
                        Some(code) => BorgError::ErrorExit {
                            code,
                            message: safe_stderr(&stderr),
                        }
                        .into(),
                        None => BorgError::Signaled.into(),
                    };
                    let _ = sender.send(Err(error)).await;
                }
                Err(error) => {
                    let _ = sender.send(Err(error.into())).await;
                }
            }
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }

    async fn command<I, S>(
        &self,
        args: I,
        cwd: Option<&Path>,
        admin: bool,
    ) -> Result<(Command, Vec<u8>)>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let passphrase_path = &self.repository.passphrase_file;
        let (passphrase_file, size) = open_passphrase(passphrase_path)?;
        let passphrase = read_passphrase(passphrase_file, size, passphrase_path)?;

        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .with_context(|| format!("failed to create Borg cache {}", self.cache_dir.display()))?;
        let cache_metadata = tokio::fs::symlink_metadata(&self.cache_dir).await?;
        ensure!(
            cache_metadata.is_dir() && !cache_metadata.file_type().is_symlink(),
            "Borg cache must be a non-symlink directory"
        );
        let mut command = Command::new(&self.repository.borg_path);
        command.args(args.into_iter().map(Into::into));
        if let Some(path) = cwd {
            command.current_dir(path);
        }
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("TZ", "UTC")
            .env("BORG_REPO", &self.repository.location)
            .env("BORG_REMOTE_PATH", &self.repository.remote_path)
            .env("BORG_EXIT_CODES", "modern")
            .env(
                "BORG_LOCK_WAIT",
                self.repository.lock_wait_seconds.to_string(),
            )
            .env("BORG_CACHE_DIR", &self.cache_dir)
            .env("BORG_BASE_DIR", &self.cache_dir)
            .env("BORG_CONFIG_DIR", self.cache_dir.join("config"))
            .env("BORG_SECURITY_DIR", self.cache_dir.join("security"))
            .env("BORG_KEYS_DIR", self.cache_dir.join("keys"))
            .env("BORG_RSH", self.borg_rsh(admin))
            .env("BORG_PASSPHRASE_FD", "0")
            .stdin(Stdio::piped())
            .kill_on_drop(true);
        Ok((command, passphrase))
    }

    fn borg_rsh(&self, admin: bool) -> String {
        let ssh_key = if admin {
            self.repository
                .maintenance_ssh_key
                .as_ref()
                .unwrap_or(&self.repository.ssh_key)
        } else {
            &self.repository.ssh_key
        };
        format!(
            "ssh -p {} -i {} -o UserKnownHostsFile={} -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes -o BatchMode=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=3",
            self.repository.ssh_port,
            ssh_key.display(),
            self.repository.known_hosts.display()
        )
    }
}

#[derive(Clone)]
pub struct BorgBackend {
    runner: BorgRunner,
}

impl BorgBackend {
    pub fn new(config: &Config) -> Self {
        Self {
            runner: BorgRunner::new(config.repository.clone(), config.backup.cache_dir.clone()),
        }
    }

    pub fn runner(&self) -> &BorgRunner {
        &self.runner
    }
}

#[async_trait]
impl Backend for BorgBackend {
    async fn preflight(&self) -> Result<()> {
        let output = require_success(self.runner.run(["--version"], None, false).await?)?;
        let version = String::from_utf8_lossy(&output.stdout);
        ensure!(
            version.contains("borg 1.4"),
            "Borg 1.4 is required; found {}",
            version.trim()
        );
        Ok(())
    }

    async fn repository_exists(&self) -> Result<bool> {
        match self.runner.run(["info"], None, false).await {
            Ok(output) => {
                require_success(output)?;
                Ok(true)
            }
            Err(error) => {
                let text = format!("{error:#}").to_ascii_lowercase();
                if text.contains("does not exist") || text.contains("repository not found") {
                    Ok(false)
                } else {
                    Err(error.context("cannot safely determine whether repository exists"))
                }
            }
        }
    }

    async fn init_repository(&self) -> Result<()> {
        if self.repository_exists().await? {
            bail!("repository already exists; refusing to initialize it");
        }
        require_success(
            self.runner
                .run(["init", "--encryption=repokey-blake2"], None, false)
                .await?,
        )?;
        Ok(())
    }

    async fn repository_identity(&self) -> Result<RepositoryIdentity> {
        let output = require_success(self.runner.run(["info", "--json"], None, false).await?)?;
        let info: RepositoryInfo = serde_json::from_slice(&output.stdout)?;
        validate_borg_id("repository id", &info.repository.id)?;
        ensure!(
            !info.repository.location.is_empty(),
            "Borg repository location is empty"
        );
        Ok(RepositoryIdentity {
            id: info.repository.id,
            location: self.runner.repository.location.clone(),
        })
    }

    async fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
        let output = require_success(self.runner.run(["list", "--json"], None, false).await?)?;
        let listed: RepositoryList = serde_json::from_slice(&output.stdout)?;
        listed.archives.into_iter().map(TryInto::try_into).collect()
    }

    async fn list_files(&self, snapshot: &str, path: Option<&str>) -> Result<FileStream> {
        validate_id("snapshot", snapshot)?;
        let stream = self
            .runner
            .stream_json_lines(
                ["list", "--json-lines", &format!("::{snapshot}")],
                false,
                parse_archive_item,
            )
            .await?;
        let filter = path.map(str::to_owned);
        if let Some(prefix) = &filter {
            validate_literal_archive_path(prefix)?;
        }
        Ok(Box::pin(stream.try_filter(move |item| {
            let included = filter
                .as_ref()
                .is_none_or(|prefix| path_matches(&item.path, prefix));
            futures::future::ready(included)
        })))
    }

    async fn create(&self, request: &CreateRequest) -> Result<Snapshot> {
        validate_id("archive name", &request.archive_name)?;
        let mut args: Vec<OsString> = vec!["create".into(), "--json".into(), "--stats".into()];
        args.push("--compression".into());
        args.push(request.compression.clone().into());
        if request.one_file_system {
            args.push("--one-file-system".into());
        }
        if request.exclude_caches {
            args.push("--exclude-caches".into());
        }
        if let Some(rate) = request.upload_rate_kib {
            args.push("--upload-ratelimit".into());
            args.push(rate.to_string().into());
        }
        for exclude in &request.excludes {
            args.push("--exclude".into());
            args.push(exclude.into());
        }
        args.push(format!("::{}", request.archive_name).into());
        args.push("--".into());
        args.extend(
            request
                .sources
                .iter()
                .map(|path| path.as_os_str().to_owned()),
        );
        let output = require_success(self.runner.run(args, None, false).await?)?;
        let created: CreateOutput = serde_json::from_slice(&output.stdout)?;
        let snapshot: Snapshot = created.archive.try_into()?;
        ensure!(
            snapshot.name == request.archive_name,
            "Borg created an unexpected archive name"
        );
        Ok(snapshot)
    }

    async fn extract(&self, snapshot: &str, paths: &[String], destination: &Path) -> Result<()> {
        validate_id("snapshot", snapshot)?;
        ensure!(!paths.is_empty(), "extract requires at least one path");
        let mut args: Vec<OsString> = vec!["extract".into()];
        for path in paths {
            validate_literal_archive_path(path)?;
            args.push("--pattern".into());
            args.push(format!("+ pp:{path}").into());
        }
        args.push("--pattern".into());
        args.push("- re:.*".into());
        args.push(format!("::{snapshot}").into());
        require_success(self.runner.run(args, Some(destination), false).await?)?;
        Ok(())
    }

    async fn mount(&self, snapshot: &str, target: &Path) -> Result<()> {
        validate_id("snapshot", snapshot)?;
        require_success(
            self.runner
                .run(
                    [
                        OsString::from("mount"),
                        OsString::from(format!("::{snapshot}")),
                        target.as_os_str().to_owned(),
                    ],
                    None,
                    false,
                )
                .await?,
        )?;
        Ok(())
    }

    async fn umount(&self, target: &Path) -> Result<()> {
        require_success(
            self.runner
                .run(
                    [OsString::from("umount"), target.as_os_str().to_owned()],
                    None,
                    false,
                )
                .await?,
        )?;
        Ok(())
    }

    async fn diff(&self, a: &str, b: &str, path: Option<&str>) -> Result<DiffStream> {
        validate_id("snapshot", a)?;
        validate_id("snapshot", b)?;
        if let Some(path) = path {
            validate_literal_archive_path(path)?;
        }
        let stream = self
            .runner
            .stream_json_lines(
                ["diff", "--json-lines", &format!("::{a}"), b],
                false,
                parse_diff_entry,
            )
            .await?;
        let filter = path.map(str::to_owned);
        Ok(Box::pin(stream.try_filter(move |entry| {
            let included = filter
                .as_ref()
                .is_none_or(|prefix| path_matches(&entry.path, prefix));
            futures::future::ready(included)
        })))
    }

    async fn prune(
        &self,
        archive_prefix: &str,
        keep: (u32, u32, u32),
        dry_run: bool,
    ) -> Result<()> {
        ensure!(
            archive_prefix.ends_with('-'),
            "archive prefix must end in '-'"
        );
        validate_id("archive prefix", archive_prefix.trim_end_matches('-'))?;
        let mut args = vec![
            "prune".to_owned(),
            "--list".to_owned(),
            "--glob-archives".to_owned(),
            format!("{archive_prefix}*"),
            "--keep-daily".to_owned(),
            keep.0.to_string(),
            "--keep-weekly".to_owned(),
            keep.1.to_string(),
            "--keep-monthly".to_owned(),
            keep.2.to_string(),
        ];
        if dry_run {
            args.push("--dry-run".into());
        }
        require_success(self.runner.run(args, None, true).await?)?;
        Ok(())
    }

    async fn compact(&self) -> Result<()> {
        require_success(self.runner.run(["compact"], None, true).await?)?;
        Ok(())
    }

    async fn check(&self, verify_data: bool) -> Result<()> {
        let mut args = vec!["check"];
        if verify_data {
            args.push("--verify-data");
        }
        require_success(self.runner.run(args, None, true).await?)?;
        Ok(())
    }

    async fn key_export(&self, destination: &Path) -> Result<()> {
        validate_export_destination(destination)?;
        ensure!(
            !destination.exists(),
            "key export destination already exists"
        );
        let parent = destination
            .parent()
            .context("key export destination has no parent")?;
        let temporary_dir = tempfile::Builder::new()
            .prefix(".boxup-key-")
            .tempdir_in(parent)?;
        let temporary = temporary_dir.path().join("repository.repokey");
        let output = require_success(
            self.runner
                .run(
                    [OsString::from("key"), OsString::from("export")],
                    None,
                    false,
                )
                .await?,
        )?;
        ensure!(!output.stdout.is_empty(), "Borg key export was empty");
        ensure!(
            output.stdout.len() <= 16 * 1024 * 1024,
            "Borg key export was unreasonably large"
        );
        std::fs::write(&temporary, output.stdout)?;
        let metadata = std::fs::symlink_metadata(&temporary)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Borg key export did not create a regular file"
        );
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&temporary, permissions)?;
        let temporary_parent = File::open(temporary_dir.path())?;
        let parent_file = File::open(parent)?;
        renameat2(
            &temporary_parent,
            temporary
                .file_name()
                .context("temporary key path has no name")?,
            &parent_file,
            destination
                .file_name()
                .context("key export destination has no name")?,
            RenameFlags::RENAME_NOREPLACE,
        )
        .context("failed to publish key export without overwrite")?;
        Ok(())
    }
}

fn open_passphrase(path: &Path) -> Result<(File, usize)> {
    let descriptor = open(
        path,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("failed to open credential {}", path.display()))?;
    let metadata = fstat(&descriptor)?;
    ensure!(
        SFlag::from_bits_truncate(metadata.st_mode).contains(SFlag::S_IFREG),
        "Borg passphrase must be a regular non-symlink file"
    );
    ensure!(
        metadata.st_uid == Uid::effective().as_raw(),
        "Borg passphrase must be owned by the effective user"
    );
    ensure!(
        metadata.st_mode & 0o077 == 0,
        "Borg passphrase must not be accessible by group or other"
    );
    ensure!(
        metadata.st_size >= 0 && metadata.st_size <= 16 * 1024,
        "Borg passphrase is unreasonably large"
    );
    Ok((File::from(descriptor), metadata.st_size as usize))
}

fn read_passphrase(mut file: File, size: usize, path: &Path) -> Result<Vec<u8>> {
    let mut passphrase = Vec::with_capacity(size);
    file.read_to_end(&mut passphrase)
        .with_context(|| format!("failed to read credential {}", path.display()))?;
    while matches!(passphrase.last(), Some(b'\n' | b'\r')) {
        passphrase.pop();
    }
    ensure!(!passphrase.is_empty(), "Borg passphrase file is empty");
    ensure!(
        passphrase.len() <= 16 * 1024,
        "Borg passphrase is unreasonably large"
    );
    ensure!(
        !passphrase.contains(&0),
        "Borg passphrase contains a NUL byte"
    );
    passphrase.push(b'\n');
    Ok(passphrase)
}

fn classify_output(code: Option<i32>, stdout: Vec<u8>, stderr: Vec<u8>) -> Result<BorgOutput> {
    match code {
        Some(0) => Ok(BorgOutput {
            stdout,
            stderr,
            exit: BorgExit::Success,
        }),
        Some(code) if is_warning_code(code) => {
            tracing::warn!("Borg completed with warnings: {}", safe_stderr(&stderr));
            Ok(BorgOutput {
                stdout,
                stderr,
                exit: BorgExit::Warning,
            })
        }
        Some(code) => Err(BorgError::ErrorExit {
            code,
            message: safe_stderr(&stderr),
        }
        .into()),
        None => Err(BorgError::Signaled.into()),
    }
}

fn is_warning_code(code: i32) -> bool {
    code == 1 || (100..=127).contains(&code)
}

fn require_success(output: BorgOutput) -> Result<BorgOutput> {
    match output.exit {
        BorgExit::Success => Ok(output),
        BorgExit::Warning => Err(BorgError::WarningExit {
            message: safe_stderr(&output.stderr),
        }
        .into()),
    }
}

fn safe_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .trim()
        .chars()
        .take(2000)
        .collect()
}

fn validate_literal_archive_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "archive path is empty");
    ensure!(!path.starts_with('/'), "archive path must be relative");
    ensure!(
        !path.chars().any(char::is_control),
        "archive path contains control characters"
    );
    ensure!(
        path.split('/')
            .all(|component| !component.is_empty() && component != "." && component != ".."),
        "archive path is not normalized"
    );
    let components: Vec<_> = Path::new(path).components().collect();
    ensure!(
        components
            .iter()
            .all(|part| matches!(part, std::path::Component::Normal(_))),
        "archive path is not normalized"
    );
    let normalized: PathBuf = components.iter().collect();
    ensure!(
        normalized == Path::new(path),
        "archive path is not normalized"
    );
    Ok(())
}

fn path_matches(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('/'))
}

#[derive(Deserialize)]
struct RepositoryList {
    archives: Vec<SnapshotRaw>,
}

#[derive(Deserialize)]
struct RepositoryInfo {
    repository: RepositoryIdentity,
}

#[derive(Deserialize)]
struct CreateOutput {
    archive: SnapshotRaw,
}

#[derive(Deserialize)]
struct SnapshotRaw {
    id: String,
    name: String,
    #[serde(deserialize_with = "deserialize_borg_time")]
    start: DateTime<Utc>,
    #[serde(default, deserialize_with = "deserialize_optional_borg_time")]
    end: Option<DateTime<Utc>>,
    hostname: Option<String>,
    username: Option<String>,
}

impl TryFrom<SnapshotRaw> for Snapshot {
    type Error = anyhow::Error;

    fn try_from(value: SnapshotRaw) -> Result<Self> {
        validate_id("snapshot", &value.name)?;
        validate_borg_id("archive id", &value.id)?;
        Ok(Self {
            id: value.id,
            name: value.name,
            start: value.start,
            end: value.end,
            hostname: value.hostname,
            username: value.username,
        })
    }
}

#[derive(Deserialize)]
struct ArchiveItemRaw {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: u64,
    #[serde(default, deserialize_with = "deserialize_optional_borg_time")]
    mtime: Option<DateTime<Utc>>,
    mode: Option<String>,
    uid: Option<u32>,
    gid: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_name")]
    user: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_name")]
    group: Option<String>,
    #[serde(alias = "linktarget")]
    link_target: Option<String>,
    #[serde(default)]
    healthy: Option<bool>,
}

fn parse_archive_item(line: &str) -> Result<ArchiveItem> {
    let raw: ArchiveItemRaw = serde_json::from_str(line)?;
    validate_literal_archive_path(&raw.path)?;
    if let Some(user) = &raw.user {
        validate_borg_name("archive user", user)?;
    }
    if let Some(group) = &raw.group {
        validate_borg_name("archive group", group)?;
    }
    Ok(ArchiveItem {
        path: raw.path,
        kind: FileType::from_borg(&raw.kind),
        size: raw.size,
        mtime: raw.mtime,
        mode: raw.mode,
        uid: raw.uid,
        gid: raw.gid,
        user: raw.user,
        group: raw.group,
        link_target: raw.link_target,
        health: raw
            .healthy
            .map(|healthy| if healthy { "healthy" } else { "damaged" }.into()),
    })
}

fn parse_diff_entry(line: &str) -> Result<DiffEntry> {
    let raw: DiffRaw = serde_json::from_str(line)?;
    validate_literal_archive_path(&raw.path)?;
    Ok(DiffEntry {
        path: raw.path,
        change: serde_json::to_string(&raw.changes)?,
    })
}

async fn spawn_with_passphrase(mut command: Command, mut passphrase: Vec<u8>) -> Result<Child> {
    let mut child = command.spawn().context("failed to execute Borg")?;
    let write_result = async {
        let mut stdin = child
            .stdin
            .take()
            .context("Borg passphrase pipe unavailable")?;
        stdin.write_all(&passphrase).await?;
        stdin.shutdown().await?;
        Result::<()>::Ok(())
    }
    .await;
    passphrase.fill(0);
    if let Err(error) = write_result {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(error).context("failed to write Borg passphrase pipe");
    }
    Ok(child)
}

#[derive(Deserialize)]
struct DiffRaw {
    path: String,
    changes: Value,
}

fn deserialize_borg_time<'de, D>(deserializer: D) -> std::result::Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_borg_time(&value).map_err(de::Error::custom)
}

fn deserialize_optional_borg_time<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| parse_borg_time(&value).map_err(de::Error::custom))
        .transpose()
}

fn parse_borg_time(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    let timestamp = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .with_context(|| format!("invalid Borg timestamp {value:?}"))?;
    Ok(timestamp.and_utc())
}

fn deserialize_optional_name<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(Value::Number(value)) if value.is_i64() || value.is_u64() => {
            Ok(Some(value.to_string()))
        }
        Some(_) => Err(de::Error::custom(
            "Borg user/group must be a string or integer",
        )),
    }
}

fn validate_borg_id(label: &str, value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid {label}"
    );
    Ok(())
}

fn validate_borg_name(label: &str, value: &str) -> Result<()> {
    ensure!(value.len() <= 1024, "{label} is too long");
    ensure!(
        !value.chars().any(char::is_control),
        "{label} contains control characters"
    );
    Ok(())
}

fn validate_export_destination(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "key export destination must be absolute"
    );
    let text = path
        .to_str()
        .context("key export destination is not UTF-8")?;
    ensure!(
        text == "/"
            || (text.starts_with('/')
                && !text.ends_with('/')
                && text
                    .split('/')
                    .skip(1)
                    .all(|part| !part.is_empty() && part != "." && part != "..")),
        "key export destination is not normalized"
    );
    ensure!(path != Path::new("/"), "key export destination cannot be /");
    let parent = path
        .parent()
        .context("key export destination has no parent")?;
    let metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "key export parent must be a non-symlink directory"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn wildcard_paths_remain_literal_prefixes() {
        assert!(path_matches("home/a*", "home/a*"));
        assert!(!path_matches("home/abc", "home/a*"));
    }

    #[test]
    fn archive_paths_must_be_normalized_and_relative() {
        assert!(validate_literal_archive_path("etc/hosts").is_ok());
        assert!(validate_literal_archive_path("../etc/hosts").is_err());
        assert!(validate_literal_archive_path("etc/./hosts").is_err());
        assert!(validate_literal_archive_path("etc//hosts").is_err());
        assert!(validate_literal_archive_path("/etc/hosts").is_err());
    }

    #[test]
    fn parses_borg_local_utc_and_rfc3339_timestamps() {
        assert_eq!(
            parse_borg_time("2026-07-22T04:00:00.123456")
                .unwrap()
                .to_rfc3339(),
            "2026-07-22T04:00:00.123456+00:00"
        );
        assert_eq!(
            parse_borg_time("2026-07-22T04:00:00").unwrap().to_rfc3339(),
            "2026-07-22T04:00:00+00:00"
        );
        assert_eq!(
            parse_borg_time("2026-07-22T06:00:00+02:00")
                .unwrap()
                .to_rfc3339(),
            "2026-07-22T04:00:00+00:00"
        );
    }

    #[test]
    fn passphrase_path_replacement_does_not_change_open_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("passphrase");
        let replacement = temp.path().join("replacement");
        std::fs::write(&path, "opened-file\n").unwrap();
        std::fs::write(&replacement, "replacement\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();

        let (file, size) = open_passphrase(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&replacement, &path).unwrap();

        assert!(read_passphrase(file, size, &path).unwrap() == b"opened-file\n");
    }

    #[test]
    fn modern_exit_codes_are_distinct() {
        assert_eq!(
            classify_output(Some(0), vec![], vec![]).unwrap().exit,
            BorgExit::Success
        );
        assert_eq!(
            classify_output(Some(1), vec![], vec![]).unwrap().exit,
            BorgExit::Warning
        );
        assert_eq!(
            classify_output(Some(100), vec![], vec![]).unwrap().exit,
            BorgExit::Warning
        );
        assert_eq!(
            classify_output(Some(127), vec![], vec![]).unwrap().exit,
            BorgExit::Warning
        );
        assert!(classify_output(Some(99), vec![], b"error".to_vec()).is_err());
        assert!(classify_output(Some(128), vec![], b"signal".to_vec()).is_err());
        assert!(classify_output(Some(2), vec![], b"error".to_vec()).is_err());
    }

    #[test]
    fn backend_operations_reject_warnings() {
        let output = classify_output(Some(100), vec![], b"warning".to_vec()).unwrap();
        assert!(require_success(output).is_err());
    }
}
