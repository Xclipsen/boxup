use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    pub version: u32,
    pub host: HostConfig,
    pub repository: RepositoryConfig,
    pub backup: BackupConfig,
    pub retention: RetentionConfig,
    pub restore: RestoreConfig,
    pub index: IndexConfig,
    pub schedule: ScheduleConfig,
    pub notifications: NotificationsConfig,
    pub docker: DockerConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseConfig {
    pub version: u32,
    pub host: String,
    pub repository_location: String,
    pub index_path: PathBuf,
    pub state_dir: PathBuf,
    pub system_profile: PathBuf,
    pub due_hours: u64,
}

impl BrowseConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read browse config {}", path.display()))?;
        ensure!(text.len() <= 64 * 1024, "browse config is too large");
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("invalid browse config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_system(config: &Config, system_profile: &Path) -> Result<Self> {
        config.validate_system_profile(system_profile)?;
        Ok(Self {
            version: CONFIG_VERSION,
            host: config.host.id.clone(),
            repository_location: config.repository.location.clone(),
            index_path: config.index.path.clone(),
            state_dir: config.backup.state_dir.clone(),
            system_profile: system_profile.to_path_buf(),
            due_hours: config.schedule.due_hours,
        })
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == CONFIG_VERSION,
            "unsupported browse config version"
        );
        validate_id("browse host", &self.host)?;
        validate_repository_location("browse repository", &self.repository_location)?;
        validate_absolute(&self.index_path)?;
        validate_absolute(&self.state_dir)?;
        validate_absolute(&self.system_profile)?;
        ensure!(
            self.index_path
                == Path::new("/var/lib/boxup-index")
                    .join(&self.host)
                    .join("index.sqlite3"),
            "browse index path is not the standard host path"
        );
        ensure!(
            self.state_dir == Path::new("/var/lib/boxup").join(&self.host),
            "browse state path is not the standard host path"
        );
        ensure!(
            self.system_profile == Path::new("/etc/boxup").join(format!("{}.toml", self.host)),
            "browse system profile path does not match host"
        );
        ensure!(
            (1..=8760).contains(&self.due_hours),
            "invalid browse due_hours"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub location: String,
    pub passphrase_file: PathBuf,
    pub ssh_key: PathBuf,
    pub maintenance_ssh_key: Option<PathBuf>,
    pub known_hosts: PathBuf,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    #[serde(default = "default_borg_path")]
    pub borg_path: PathBuf,
    #[serde(default = "default_remote_path")]
    pub remote_path: String,
    #[serde(default = "default_lock_wait")]
    pub lock_wait_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    pub sources: Vec<PathBuf>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default = "default_true")]
    pub one_file_system: bool,
    #[serde(default = "default_true")]
    pub exclude_caches: bool,
    #[serde(default = "default_compression")]
    pub compression: String,
    pub upload_rate_kib: Option<u64>,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    #[serde(default = "default_daily")]
    pub keep_daily: u32,
    #[serde(default = "default_weekly")]
    pub keep_weekly: u32,
    #[serde(default = "default_monthly")]
    pub keep_monthly: u32,
    #[serde(default = "default_recent_backup_hours")]
    pub require_backup_within_hours: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreConfig {
    pub staging_dir: PathBuf,
    #[serde(default)]
    pub denied_paths: Vec<PathBuf>,
    #[serde(default = "default_restore_files")]
    pub max_files: u64,
    #[serde(default = "default_restore_bytes")]
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    #[serde(default = "default_schedule_mode")]
    pub mode: ScheduleMode,
    #[serde(default = "default_due_hours")]
    pub due_hours: u64,
    pub calendar: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    Due,
    Calendar,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    #[serde(default)]
    pub enabled: bool,
    pub discord_webhook_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    #[serde(default)]
    pub enabled: bool,
    pub staging_dir: Option<PathBuf>,
    #[serde(default)]
    pub stop_containers: Vec<String>,
    #[serde(default)]
    pub stop_all_stateful: bool,
    #[serde(default)]
    pub stage_mounts: Vec<PathBuf>,
    #[serde(default)]
    pub postgres_users: BTreeMap<String, String>,
    #[serde(default)]
    pub stop_services: Vec<String>,
    #[serde(default)]
    pub service_paths: Vec<PathBuf>,
    #[serde(default = "default_docker_min_free")]
    pub min_free_bytes: u64,
    #[serde(default = "default_docker_bin")]
    pub docker_path: PathBuf,
    #[serde(default = "default_rsync_bin")]
    pub rsync_path: PathBuf,
    #[serde(default = "default_systemctl_bin")]
    pub systemctl_path: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect config {}", path.display()))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "config must be a regular non-symlink file"
        );
        ensure!(metadata.len() <= 1024 * 1024, "config file is too large");
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
        config.source_path = Some(
            path.canonicalize()
                .with_context(|| format!("failed to canonicalize config {}", path.display()))?,
        );
        config.expand_home()?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == CONFIG_VERSION,
            "unsupported config version {}; expected {}",
            self.version,
            CONFIG_VERSION
        );
        validate_id("host id", &self.host.id)?;
        ensure!(
            self.host.id.len() <= 24,
            "host id must be at most 24 characters"
        );
        validate_repository_location("repository location", &self.repository.location)?;
        validate_remote_path(&self.repository.remote_path)?;
        ensure!(self.repository.ssh_port > 0, "invalid SSH port");
        ensure!(
            (1..=3600).contains(&self.repository.lock_wait_seconds),
            "repository.lock_wait_seconds must be between 1 and 3600"
        );
        validate_safe_ssh_path(&self.repository.ssh_key)?;
        validate_safe_ssh_path(&self.repository.known_hosts)?;
        if let Some(path) = &self.repository.maintenance_ssh_key {
            validate_safe_ssh_path(path)?;
        }
        for path in [
            &self.repository.passphrase_file,
            &self.repository.ssh_key,
            &self.repository.known_hosts,
            &self.repository.borg_path,
            &self.backup.state_dir,
            &self.backup.cache_dir,
            &self.restore.staging_dir,
            &self.index.path,
        ] {
            validate_absolute(path)?;
        }
        for path in [
            &self.backup.state_dir,
            &self.backup.cache_dir,
            &self.restore.staging_dir,
            &self.index.path,
        ] {
            ensure!(
                path != Path::new("/"),
                "mutable paths may not be filesystem root"
            );
        }
        if let Some(path) = &self.source_path {
            validate_absolute(path)?;
        }
        ensure!(
            !self.backup.sources.is_empty(),
            "backup.sources cannot be empty"
        );
        ensure!(
            self.backup.sources.len() <= 1024,
            "backup.sources contains too many entries"
        );
        ensure!(
            self.backup.excludes.len() <= 10_000,
            "backup.excludes contains too many entries"
        );
        for source in &self.backup.sources {
            validate_absolute(source)?;
            ensure!(source != Path::new("/"), "backup source / is not supported");
        }
        for value in &self.backup.excludes {
            validate_text("exclude", value)?;
        }
        ensure!(
            matches!(
                self.backup.compression.as_str(),
                "none" | "lz4" | "zstd" | "zstd,1" | "zstd,3" | "auto,lzma"
            ),
            "unsupported compression setting"
        );
        ensure!(
            self.backup
                .upload_rate_kib
                .is_none_or(|rate| rate <= 10_000_000),
            "backup.upload_rate_kib is unreasonable"
        );
        ensure!(
            self.retention.keep_daily + self.retention.keep_weekly + self.retention.keep_monthly
                > 0,
            "retention must keep at least one archive"
        );
        ensure!(
            self.retention.keep_daily <= 366,
            "keep_daily is unreasonable"
        );
        ensure!(
            self.retention.keep_weekly <= 260,
            "keep_weekly is unreasonable"
        );
        ensure!(
            self.retention.keep_monthly <= 240,
            "keep_monthly is unreasonable"
        );
        ensure!(
            self.restore.max_files > 0,
            "restore.max_files must be positive"
        );
        ensure!(
            self.restore.max_files <= 10_000_000,
            "restore.max_files is unreasonable"
        );
        ensure!(
            self.restore.max_bytes > 0,
            "restore.max_bytes must be positive"
        );
        ensure!(
            self.restore.max_bytes <= 10 * 1024 * 1024 * 1024 * 1024 * 1024,
            "restore.max_bytes is unreasonable"
        );
        for denied in &self.restore.denied_paths {
            validate_absolute(denied)?;
        }
        let distinct = [
            &self.backup.state_dir,
            &self.backup.cache_dir,
            &self.restore.staging_dir,
            &self.index.path,
        ];
        let normalized: HashSet<_> = distinct.iter().map(|p| lexical_path(p)).collect();
        ensure!(
            normalized.len() == distinct.len(),
            "state/cache/index/staging paths must be distinct"
        );
        for (position, path) in distinct.iter().enumerate() {
            for other in distinct.iter().skip(position + 1) {
                ensure!(
                    !path.starts_with(other) && !other.starts_with(path),
                    "state/cache/index/staging paths may not overlap"
                );
            }
        }
        if self.schedule.mode == ScheduleMode::Calendar {
            let calendar = self.schedule.calendar.as_deref().unwrap_or_default();
            ensure!(
                !calendar.trim().is_empty(),
                "calendar mode requires schedule.calendar"
            );
            validate_calendar(calendar)?;
        } else {
            ensure!(
                self.schedule.calendar.is_none(),
                "due schedule must not set schedule.calendar"
            );
        }
        ensure!(
            (1..=8760).contains(&self.schedule.due_hours),
            "schedule.due_hours must be between 1 and 8760"
        );
        ensure!(
            (1..=8760).contains(&self.retention.require_backup_within_hours),
            "retention.require_backup_within_hours must be between 1 and 8760"
        );
        if self.notifications.enabled {
            let path = self
                .notifications
                .discord_webhook_file
                .as_ref()
                .context("notifications enabled without discord_webhook_file")?;
            validate_absolute(path)?;
        }
        if self.docker.enabled {
            let staging = self
                .docker
                .staging_dir
                .as_ref()
                .context("docker enabled without staging_dir")?;
            validate_absolute(staging)?;
            ensure!(
                staging != Path::new("/"),
                "Docker staging may not be filesystem root"
            );
            ensure!(
                lexical_path(staging) != lexical_path(&self.restore.staging_dir),
                "Docker and restore staging directories must differ"
            );
            for path in distinct {
                ensure!(
                    !staging.starts_with(path) && !path.starts_with(staging),
                    "Docker staging may not overlap state/cache/index/restore paths"
                );
            }
            validate_absolute(&self.docker.docker_path)?;
            validate_absolute(&self.docker.rsync_path)?;
            validate_absolute(&self.docker.systemctl_path)?;
            ensure!(
                self.docker.min_free_bytes > 0,
                "docker.min_free_bytes must be positive"
            );
            ensure!(
                self.docker.stop_containers.len() <= 100_000,
                "docker.stop_containers contains too many entries"
            );
            for id in &self.docker.stop_containers {
                validate_id("container id/name", id)?;
            }
            ensure!(
                self.docker.stage_mounts.len() <= 100_000,
                "docker.stage_mounts contains too many entries"
            );
            validate_unique_paths("docker.stage_mounts", &self.docker.stage_mounts)?;
            ensure!(
                self.docker.postgres_users.len() <= 100_000,
                "docker.postgres_users contains too many entries"
            );
            for (container, role) in &self.docker.postgres_users {
                validate_container_key(container)?;
                validate_postgres_role(role)?;
            }
            ensure!(
                self.docker.stop_services.len() <= 10_000,
                "docker.stop_services contains too many entries"
            );
            let mut services = HashSet::new();
            for service in &self.docker.stop_services {
                validate_service_name(service)?;
                ensure!(
                    services.insert(service),
                    "docker.stop_services contains duplicate entries"
                );
            }
            ensure!(
                self.docker.service_paths.len() <= 10_000,
                "docker.service_paths contains too many entries"
            );
            validate_unique_paths("docker.service_paths", &self.docker.service_paths)?;
        }
        Ok(())
    }

    pub fn validate_system_profile(&self, path: &Path) -> Result<()> {
        validate_absolute(path)?;
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .context("system profile filename is not UTF-8")?;
        ensure!(
            stem == self.host.id,
            "system profile filename must equal host.id"
        );
        ensure!(
            path == Path::new("/etc/boxup").join(format!("{}.toml", self.host.id)),
            "system profile must be /etc/boxup/HOST.toml"
        );
        let host = &self.host.id;
        ensure!(
            self.repository.passphrase_file
                == Path::new("/etc/boxup").join(format!("{host}.passphrase")),
            "system passphrase path must be /etc/boxup/HOST.passphrase"
        );
        ensure!(
            self.repository.ssh_key == Path::new("/etc/boxup").join(format!("{host}_ed25519")),
            "system SSH key path must be /etc/boxup/HOST_ed25519"
        );
        if let Some(path) = &self.repository.maintenance_ssh_key {
            ensure!(
                path == &Path::new("/etc/boxup").join(format!("{host}_maintenance_ed25519")),
                "system maintenance key path must be /etc/boxup/HOST_maintenance_ed25519"
            );
        }
        ensure!(
            self.repository.known_hosts == Path::new("/etc/boxup/known_hosts"),
            "system known_hosts path must be /etc/boxup/known_hosts"
        );
        ensure!(
            self.repository.borg_path == Path::new("/usr/bin/borg"),
            "system Borg path must be /usr/bin/borg"
        );
        ensure!(
            self.backup.state_dir == Path::new("/var/lib/boxup").join(host),
            "system state_dir must be /var/lib/boxup/HOST"
        );
        ensure!(
            self.backup.cache_dir == Path::new("/var/cache/boxup").join(host),
            "system cache_dir must be /var/cache/boxup/HOST"
        );
        ensure!(
            self.restore.staging_dir == Path::new("/var/lib/boxup-restore").join(host),
            "system restore staging_dir must be /var/lib/boxup-restore/HOST"
        );
        ensure!(
            self.index.path
                == Path::new("/var/lib/boxup-index")
                    .join(host)
                    .join("index.sqlite3"),
            "system index path must be /var/lib/boxup-index/HOST/index.sqlite3"
        );
        if self.docker.enabled {
            ensure!(
                self.docker.staging_dir.as_deref()
                    == Some(Path::new("/var/lib/boxup-docker").join(host).as_path()),
                "system Docker staging_dir must be /var/lib/boxup-docker/HOST"
            );
            ensure!(
                self.docker.docker_path == Path::new("/usr/bin/docker"),
                "system Docker path must be /usr/bin/docker"
            );
            ensure!(
                self.docker.rsync_path == Path::new("/usr/bin/rsync"),
                "system rsync path must be /usr/bin/rsync"
            );
            ensure!(
                self.docker.systemctl_path == Path::new("/usr/bin/systemctl"),
                "system systemctl path must be /usr/bin/systemctl"
            );
        }
        if let Some(path) = &self.notifications.discord_webhook_file {
            ensure!(
                path == &Path::new("/etc/boxup").join(format!("{host}.discord-webhook")),
                "system webhook path must be /etc/boxup/HOST.discord-webhook"
            );
        }
        Ok(())
    }

    pub fn archive_prefix(&self) -> String {
        format!("{}-", self.host.id)
    }

    fn expand_home(&mut self) -> Result<()> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let expand = |path: &mut PathBuf| -> Result<()> {
            if path == Path::new("~") || path.starts_with("~/") {
                let base = home
                    .as_ref()
                    .context("HOME is unavailable for ~ expansion")?;
                let suffix = path.strip_prefix("~").expect("prefix checked");
                *path = base.join(suffix);
            }
            Ok(())
        };
        expand(&mut self.repository.passphrase_file)?;
        expand(&mut self.repository.ssh_key)?;
        if let Some(path) = &mut self.repository.maintenance_ssh_key {
            expand(path)?;
        }
        expand(&mut self.repository.known_hosts)?;
        expand(&mut self.repository.borg_path)?;
        for source in &mut self.backup.sources {
            expand(source)?;
        }
        expand(&mut self.backup.state_dir)?;
        expand(&mut self.backup.cache_dir)?;
        expand(&mut self.restore.staging_dir)?;
        for path in &mut self.restore.denied_paths {
            expand(path)?;
        }
        expand(&mut self.index.path)?;
        if let Some(path) = &mut self.notifications.discord_webhook_file {
            expand(path)?;
        }
        if let Some(path) = &mut self.docker.staging_dir {
            expand(path)?;
        }
        expand(&mut self.docker.docker_path)?;
        expand(&mut self.docker.rsync_path)?;
        expand(&mut self.docker.systemctl_path)?;
        for path in &mut self.docker.stage_mounts {
            expand(path)?;
        }
        for path in &mut self.docker.service_paths {
            expand(path)?;
        }
        Ok(())
    }
}

pub fn validate_id(label: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty() && value.len() <= 64, "invalid {label}");
    ensure!(
        value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "{label} may contain only ASCII letters, digits, '-' and '_'"
    );
    ensure!(
        value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric),
        "{label} must begin and end with an ASCII letter or digit"
    );
    Ok(())
}

pub fn validate_text(label: &str, value: &str) -> Result<()> {
    ensure!(
        !value.chars().any(char::is_control),
        "{label} contains control characters"
    );
    Ok(())
}

fn validate_unique_paths(label: &str, paths: &[PathBuf]) -> Result<()> {
    let mut unique = HashSet::new();
    for path in paths {
        validate_absolute(path)?;
        ensure!(
            path != Path::new("/"),
            "{label} may not contain filesystem root"
        );
        ensure!(unique.insert(path), "{label} contains duplicate paths");
    }
    Ok(())
}

fn validate_container_key(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 128,
        "invalid PostgreSQL container name or id"
    );
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "PostgreSQL container name or id contains unsafe characters"
    );
    ensure!(
        value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric),
        "PostgreSQL container name or id must begin and end with an ASCII letter or digit"
    );
    Ok(())
}

fn validate_postgres_role(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 63,
        "invalid PostgreSQL role"
    );
    ensure!(
        !value.starts_with('-')
            && value
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') }),
        "PostgreSQL role contains unsafe characters"
    );
    Ok(())
}

pub(crate) fn validate_service_name(value: &str) -> Result<()> {
    let stem = value.strip_suffix(".service").unwrap_or_default();
    ensure!(
        !stem.is_empty() && value.len() <= 256 && value.is_ascii(),
        "invalid systemd service name"
    );
    ensure!(
        stem.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b':')
        }),
        "systemd service name contains unsafe characters"
    );
    ensure!(
        stem.as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric),
        "systemd service name must begin with an ASCII letter or digit"
    );
    Ok(())
}

fn validate_repository_location(label: &str, value: &str) -> Result<()> {
    validate_text(label, value)?;
    ensure!(!value.trim().is_empty(), "{label} cannot be empty");
    ensure!(
        !value.chars().any(char::is_whitespace),
        "{label} contains whitespace"
    );
    ensure!(
        !value.contains('?') && !value.contains('#'),
        "{label} may not contain a query or fragment"
    );
    if let Some((_, remainder)) = value.split_once("://") {
        let authority = remainder.split('/').next().unwrap_or(remainder);
        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            ensure!(
                !userinfo.contains(':'),
                "{label} may not contain an embedded password"
            );
        }
    }
    Ok(())
}

pub fn validate_absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "path must be absolute: {}",
        path.display()
    );
    let text = path.to_str().context("path is not UTF-8")?;
    validate_text("path", text)?;
    ensure!(
        text == "/"
            || (text.starts_with('/')
                && !text.ends_with('/')
                && text
                    .split('/')
                    .skip(1)
                    .all(|part| !part.is_empty() && part != "." && part != "..")),
        "path is not lexically normalized: {}",
        path.display()
    );
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
        "path contains unsupported components: {}",
        path.display()
    );
    Ok(())
}

fn validate_remote_path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256,
        "invalid remote path"
    );
    ensure!(
        value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+')
        }),
        "repository.remote_path contains unsafe characters"
    );
    ensure!(
        !value.starts_with('-')
            && !value.ends_with('/')
            && value
                .split('/')
                .skip(usize::from(value.starts_with('/')))
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "repository.remote_path is not a safe executable token/path"
    );
    Ok(())
}

pub fn validate_calendar(value: &str) -> Result<()> {
    validate_text("calendar", value)?;
    let mut fields = value.split(' ');
    ensure!(fields.next() == Some("*-*-*"), "unsupported calendar date");
    let time = fields.next().context("calendar time is missing")?;
    ensure!(
        fields.next() == Some("UTC"),
        "calendar timezone must be UTC"
    );
    ensure!(fields.next().is_none(), "calendar has extra fields");
    let parts: Vec<_> = time.split(':').collect();
    ensure!(
        parts.len() == 3 && parts[2] == "00",
        "calendar must use HH:MM:00"
    );
    let hour: u8 = parts[0].parse().context("invalid calendar hour")?;
    let minute: u8 = parts[1].parse().context("invalid calendar minute")?;
    ensure!(hour < 24 && minute < 60, "calendar time is out of range");
    ensure!(
        parts[0].len() == 2 && parts[1].len() == 2,
        "calendar time must be zero-padded"
    );
    Ok(())
}

fn validate_safe_ssh_path(path: &Path) -> Result<()> {
    validate_absolute(path)?;
    let text = path.to_str().context("SSH path is not UTF-8")?;
    if !text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+' | b'@'))
    {
        bail!("SSH paths may not contain whitespace or shell metacharacters: {text}");
    }
    Ok(())
}

fn lexical_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn default_true() -> bool {
    true
}
fn default_ssh_port() -> u16 {
    22
}
fn default_borg_path() -> PathBuf {
    PathBuf::from("/usr/bin/borg")
}
fn default_remote_path() -> String {
    "borg-1.4".into()
}
fn default_lock_wait() -> u64 {
    30
}
fn default_compression() -> String {
    "zstd,3".into()
}
fn default_daily() -> u32 {
    7
}
fn default_weekly() -> u32 {
    4
}
fn default_monthly() -> u32 {
    12
}
fn default_recent_backup_hours() -> u64 {
    48
}
fn default_restore_files() -> u64 {
    100_000
}
fn default_restore_bytes() -> u64 {
    100 * 1024 * 1024 * 1024
}
fn default_schedule_mode() -> ScheduleMode {
    ScheduleMode::Due
}
fn default_due_hours() -> u64 {
    20
}
fn default_docker_min_free() -> u64 {
    10 * 1024 * 1024 * 1024
}
fn default_docker_bin() -> PathBuf {
    PathBuf::from("/usr/bin/docker")
}
fn default_rsync_bin() -> PathBuf {
    PathBuf::from("/usr/bin/rsync")
}
fn default_systemctl_bin() -> PathBuf {
    PathBuf::from("/usr/bin/systemctl")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> Config {
        Config {
            source_path: None,
            version: 1,
            host: HostConfig {
                id: "host_1".into(),
            },
            repository: RepositoryConfig {
                location: "ssh://backup@example/repo".into(),
                passphrase_file: "/etc/boxup/host_1.passphrase".into(),
                ssh_key: "/etc/boxup/id_ed25519".into(),
                maintenance_ssh_key: None,
                known_hosts: "/etc/boxup/known_hosts".into(),
                ssh_port: 22,
                borg_path: "/usr/bin/borg".into(),
                remote_path: "borg-1.4".into(),
                lock_wait_seconds: 30,
            },
            backup: BackupConfig {
                sources: vec!["/home".into()],
                excludes: vec![],
                one_file_system: true,
                exclude_caches: true,
                compression: "zstd,3".into(),
                upload_rate_kib: Some(8192),
                state_dir: "/var/lib/boxup".into(),
                cache_dir: "/var/cache/boxup".into(),
            },
            retention: RetentionConfig {
                keep_daily: 7,
                keep_weekly: 4,
                keep_monthly: 12,
                require_backup_within_hours: 48,
            },
            restore: RestoreConfig {
                staging_dir: "/var/lib/boxup-restore".into(),
                denied_paths: vec!["/etc/boxup".into()],
                max_files: 100,
                max_bytes: 1024,
            },
            index: IndexConfig {
                path: "/var/lib/boxup-index/index.sqlite3".into(),
            },
            schedule: ScheduleConfig {
                mode: ScheduleMode::Due,
                due_hours: 20,
                calendar: None,
            },
            notifications: NotificationsConfig {
                enabled: false,
                discord_webhook_file: None,
            },
            docker: DockerConfig {
                enabled: false,
                staging_dir: None,
                stop_containers: vec![],
                stop_all_stateful: false,
                stage_mounts: vec![],
                postgres_users: BTreeMap::new(),
                stop_services: vec![],
                service_paths: vec![],
                min_free_bytes: 1,
                docker_path: "/usr/bin/docker".into(),
                rsync_path: "/usr/bin/rsync".into(),
                systemctl_path: "/usr/bin/systemctl".into(),
            },
        }
    }

    #[test]
    fn rejects_unknown_toml_fields() {
        let text = "version=1\nextra=true";
        assert!(toml::from_str::<Config>(text).is_err());
    }

    #[test]
    fn validates_distinct_paths() {
        let mut config = valid_config();
        config.restore.staging_dir = config.backup.state_dir.clone();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_archive_prefix_metacharacters() {
        let mut config = valid_config();
        config.host.id = "bad*host".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_remote_executable_without_shell_syntax() {
        let mut config = valid_config();
        for remote in ["borg", "borg-1.4", "/usr/local/bin/borg-1.4"] {
            config.repository.remote_path = remote.into();
            config.validate().unwrap();
        }
        for remote in ["borg 1.4", "borg;id", "../borg", "-borg"] {
            config.repository.remote_path = remote.into();
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn repository_locations_cannot_embed_credentials() {
        let mut config = valid_config();
        config.repository.location = "ssh://user:password@example/repo".into();
        assert!(config.validate().is_err());
        config.repository.location = "ssh://user@example/repo".into();
        config.validate().unwrap();
    }

    #[test]
    fn rejects_root_and_slashdot_backup_sources() {
        let mut config = valid_config();
        config.backup.sources = vec!["/".into()];
        assert!(config.validate().is_err());
        config.backup.sources = vec!["/home/./user".into()];
        assert!(config.validate().is_err());
        config.backup.sources = vec!["/home//user".into()];
        assert!(config.validate().is_err());
    }

    #[test]
    fn docker_extension_validation_rejects_unsafe_and_duplicate_values() {
        let enabled = || {
            let mut config = valid_config();
            config.docker.enabled = true;
            config.docker.staging_dir = Some("/var/lib/boxup-docker".into());
            config
        };

        let mut config = enabled();
        config.docker.stage_mounts = vec!["/srv/data".into(), "/srv/data".into()];
        assert!(config.validate().is_err());

        let mut config = enabled();
        config.docker.stage_mounts = vec!["/srv/../data".into()];
        assert!(config.validate().is_err());

        let mut config = enabled();
        config
            .docker
            .postgres_users
            .insert("database".into(), "backup_role".into());
        config.validate().unwrap();
        config
            .docker
            .postgres_users
            .insert("database".into(), "bad role;".into());
        assert!(config.validate().is_err());

        let mut config = enabled();
        config.docker.stop_services = vec!["app.service".into()];
        config.docker.service_paths = vec!["/srv/app".into()];
        config.validate().unwrap();
        config.docker.stop_services = vec!["app.service;reboot".into()];
        assert!(config.validate().is_err());

        let mut config = enabled();
        config.docker.service_paths = vec!["/srv/app".into(), "/srv/app".into()];
        assert!(config.validate().is_err());

        let mut config = enabled();
        config.docker.systemctl_path = "systemctl".into();
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.docker.systemctl_path = "relative-systemctl".into();
        config.docker.stop_services = vec!["unsafe".into()];
        config.validate().unwrap();
    }

    #[test]
    fn calendar_is_strict_and_safe_for_a_timer_dropin() {
        for value in ["*-*-* 04:00:00 UTC", "*-*-* 23:59:00 UTC"] {
            validate_calendar(value).unwrap();
        }
        for value in [
            "daily",
            "*-*-* 24:00:00 UTC",
            "*-*-* 4:00:00 UTC",
            "*-*-* 04:00:01 UTC",
            "*-*-* 04:00:00 UTC\nOnFailure=x",
        ] {
            assert!(validate_calendar(value).is_err());
        }
    }

    #[test]
    fn system_profile_name_and_paths_must_match_host() {
        let mut config: Config = toml::from_str(include_str!("../examples/desktop.toml")).unwrap();
        config
            .validate_system_profile(Path::new("/etc/boxup/desktop.toml"))
            .unwrap();
        assert!(
            config
                .validate_system_profile(Path::new("/etc/boxup/other.toml"))
                .is_err()
        );
        config.index.path = "/tmp/index.sqlite3".into();
        assert!(
            config
                .validate_system_profile(Path::new("/etc/boxup/desktop.toml"))
                .is_err()
        );
        let mut config: Config = toml::from_str(include_str!("../examples/desktop.toml")).unwrap();
        config.repository.borg_path = "/usr/local/bin/borg".into();
        assert!(
            config
                .validate_system_profile(Path::new("/etc/boxup/desktop.toml"))
                .is_err()
        );
    }

    #[test]
    fn browse_descriptor_contains_no_credential_paths() {
        let config: Config = toml::from_str(include_str!("../examples/desktop.toml")).unwrap();
        let browse =
            BrowseConfig::from_system(&config, Path::new("/etc/boxup/desktop.toml")).unwrap();
        let serialized = toml::to_string(&browse).unwrap();
        assert!(!serialized.contains("passphrase"));
        assert!(!serialized.contains("ed25519"));
        assert!(!serialized.contains("known_hosts"));
        browse.validate().unwrap();
    }

    #[test]
    fn shipped_examples_are_valid_strict_configs() {
        for text in [
            include_str!("../examples/desktop.toml"),
            include_str!("../examples/ubuntu-docker-vps.toml"),
        ] {
            let config: Config = toml::from_str(text).unwrap();
            config.validate().unwrap();
        }
    }
}
