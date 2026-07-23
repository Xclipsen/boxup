use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use futures::TryStreamExt;
use nix::errno::Errno;
use nix::fcntl::{OFlag, RenameFlags, openat, renameat, renameat2};
use nix::sys::stat::{Mode, fchmod, futimens, mkdirat};
use nix::sys::time::TimeSpec;
use nix::unistd::{Gid, Uid, fchown};
use tempfile::Builder;

use crate::backend::Backend;
use crate::config::{Config, validate_absolute};
use crate::domain::{ArchiveItem, FileType};
use crate::jobs::{LocalLock, LockMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub snapshot: String,
    pub snapshot_id: String,
    pub paths: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    manifest: BTreeMap<String, ArchiveItem>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestorePhase {
    Validating,
    Extracting,
    Verifying,
    Publishing,
    Complete,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RestoreProgress {
    pub phase: RestorePhase,
    pub current: u64,
    pub total: u64,
    pub files: u64,
    pub bytes: u64,
}

pub async fn restore<B: Backend + ?Sized>(
    backend: &B,
    config: &Config,
    snapshot: &str,
    inputs: &[String],
    target: &Path,
) -> Result<RestorePlan> {
    let paths = normalize_paths(config, inputs)?;
    validate_restore_target(target)?;
    ensure_target_not_denied(config, target)?;
    create_directory_all_no_follow(&config.backup.state_dir, Mode::from_bits_truncate(0o700))?;
    let _lock = LocalLock::acquire(&config.backup.state_dir, LockMode::Shared)?;
    let plan = validate_live_selection(backend, config, snapshot, &paths).await?;
    validate_new_target(target)?;
    ensure_no_symlink_components(target.parent().context("restore target has no parent")?)?;

    create_directory_all_no_follow(&config.restore.staging_dir, Mode::from_bits_truncate(0o700))
        .with_context(|| {
            format!(
                "failed to create restore staging {}",
                config.restore.staging_dir.display()
            )
        })?;
    let target_parent = target.parent().context("restore target has no parent")?;
    let staging_directory = open_directory_no_follow(&config.restore.staging_dir)?;
    let target_directory = open_directory_no_follow(target_parent)
        .context("restore target parent does not exist or is unsafe")?;
    let staging_device = staging_directory.metadata()?.dev();
    let target_device = target_directory.metadata()?.dev();
    ensure!(
        staging_device == target_device,
        "restore staging and target must be on the same filesystem for atomic publication"
    );

    let staging = Builder::new()
        .prefix("restore-")
        .tempdir_in(&config.restore.staging_dir)?;
    revalidate_snapshot(backend, &plan).await?;
    backend
        .extract(&plan.snapshot, &paths, staging.path())
        .await?;
    verify_extracted_manifest(staging.path(), &plan, config)?;
    revalidate_snapshot(backend, &plan).await?;
    publish_directory(staging.path(), target)?;
    Ok(plan)
}

pub async fn restore_overwrite_root<B: Backend + ?Sized>(
    backend: &B,
    config: &Config,
    snapshot: &str,
    inputs: &[String],
) -> Result<RestorePlan> {
    ensure!(
        nix::unistd::Uid::effective().is_root(),
        "overwrite restore requires root"
    );
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "overwrite restore requires a TTY"
    );
    let paths = normalize_paths(config, inputs)?;
    create_directory_all_no_follow(&config.backup.state_dir, Mode::from_bits_truncate(0o700))?;
    let _lock = LocalLock::acquire(&config.backup.state_dir, LockMode::Shared)?;
    let plan = validate_live_selection(backend, config, snapshot, &paths).await?;
    println!(
        "Dry run: {} paths, {} bytes from {} would be merged into /.",
        plan.files, plan.bytes, plan.snapshot
    );
    print!("Type 'RESTORE {} TO /' to continue: ", snapshot);
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    ensure!(
        answer.trim_end() == format!("RESTORE {snapshot} TO /"),
        "confirmation did not match"
    );

    create_directory_all_no_follow(&config.restore.staging_dir, Mode::from_bits_truncate(0o700))?;
    ensure!(
        open_directory_no_follow(&config.restore.staging_dir)?
            .metadata()?
            .dev()
            == File::open("/")?.metadata()?.dev(),
        "overwrite staging must be on the root filesystem"
    );
    let staging = Builder::new()
        .prefix("overwrite-")
        .tempdir_in(&config.restore.staging_dir)?;
    revalidate_snapshot(backend, &plan).await?;
    backend
        .extract(&plan.snapshot, &paths, staging.path())
        .await?;
    verify_extracted_manifest(staging.path(), &plan, config)?;
    revalidate_snapshot(backend, &plan).await?;
    merge_without_following(staging.path(), Path::new("/"))?;
    Ok(plan)
}

pub async fn restore_original_root<B, F>(
    backend: &B,
    config: &Config,
    snapshot: &str,
    inputs: &[String],
    progress: F,
) -> Result<RestorePlan>
where
    B: Backend + ?Sized,
    F: Fn(RestoreProgress),
{
    ensure!(
        nix::unistd::Uid::effective().is_root(),
        "original-path restore requires root"
    );
    let paths = minimal_restore_paths(normalize_paths(config, inputs)?);
    for path in &paths {
        ensure_not_denied(config, path, false)?;
    }
    create_directory_all_no_follow(&config.backup.state_dir, Mode::from_bits_truncate(0o700))?;
    let _lock = LocalLock::acquire(&config.backup.state_dir, LockMode::Shared)?;
    progress(RestoreProgress {
        phase: RestorePhase::Validating,
        current: 0,
        total: 0,
        files: 0,
        bytes: 0,
    });
    let plan =
        validate_live_selection_with_progress(backend, config, snapshot, &paths, |scanned| {
            progress(RestoreProgress {
                phase: RestorePhase::Validating,
                current: scanned,
                total: 0,
                files: 0,
                bytes: 0,
            });
        })
        .await?;

    let publication_paths = original_publication_paths(&paths)?;
    let staging_root = original_restore_staging(config, &publication_paths)?;

    let staging = Builder::new()
        .prefix("original-")
        .tempdir_in(&staging_root)?;
    revalidate_snapshot(backend, &plan).await?;
    let (progress_sender, mut progress_receiver) =
        tokio::sync::watch::channel(crate::domain::ExtractProgress {
            current: 0,
            total: plan.bytes.max(1),
        });
    progress(RestoreProgress {
        phase: RestorePhase::Extracting,
        current: 0,
        total: plan.bytes,
        files: plan.files,
        bytes: plan.bytes,
    });
    let plan_snapshot = plan.snapshot.clone();
    let extraction =
        backend.extract_with_progress(&plan_snapshot, &paths, staging.path(), progress_sender);
    tokio::pin!(extraction);
    loop {
        tokio::select! {
            result = &mut extraction => {
                result?;
                break;
            }
            changed = progress_receiver.changed() => {
                if changed.is_ok() {
                    let value = *progress_receiver.borrow_and_update();
                    progress(RestoreProgress {
                        phase: RestorePhase::Extracting,
                        current: value.current,
                        total: value.total,
                        files: plan.files,
                        bytes: plan.bytes,
                    });
                }
            }
        }
    }
    progress(RestoreProgress {
        phase: RestorePhase::Verifying,
        current: 0,
        total: plan.files,
        files: plan.files,
        bytes: plan.bytes,
    });
    verify_extracted_manifest(staging.path(), &plan, config)?;
    revalidate_snapshot(backend, &plan).await?;

    for (position, path) in publication_paths.iter().enumerate() {
        progress(RestoreProgress {
            phase: RestorePhase::Publishing,
            current: position as u64,
            total: publication_paths.len() as u64,
            files: plan.files,
            bytes: plan.bytes,
        });
        publish_original_path(&staging.path().join(path), &Path::new("/").join(path))?;
    }
    progress(RestoreProgress {
        phase: RestorePhase::Complete,
        current: publication_paths.len() as u64,
        total: publication_paths.len() as u64,
        files: plan.files,
        bytes: plan.bytes,
    });
    Ok(plan)
}

fn minimal_restore_paths(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    let mut minimal: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        if !minimal
            .iter()
            .any(|ancestor| archive_path_matches(&path, ancestor))
        {
            minimal.push(path);
        }
    }
    minimal
}

fn original_publication_paths(paths: &[String]) -> Result<Vec<String>> {
    let mut publication = Vec::with_capacity(paths.len());
    for path in paths {
        let mut candidate = Path::new("/").join(path);
        let mut root = path.clone();
        loop {
            match fs::symlink_metadata(&candidate) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    root = candidate
                        .strip_prefix("/")?
                        .to_str()
                        .context("original restore target is not UTF-8")?
                        .to_owned();
                    candidate = candidate
                        .parent()
                        .context("original restore target has no existing ancestor")?
                        .to_path_buf();
                }
                Err(error) => return Err(error.into()),
            }
        }
        ensure_no_symlink_components(&candidate)?;
        ensure!(
            candidate.is_dir(),
            "original restore target parent is not a directory: {}",
            candidate.display()
        );
        publication.push(root);
    }
    Ok(minimal_restore_paths(publication))
}

fn original_restore_staging(config: &Config, paths: &[String]) -> Result<PathBuf> {
    create_directory_all_no_follow(&config.restore.staging_dir, Mode::from_bits_truncate(0o700))?;
    let configured_device = open_directory_no_follow(&config.restore.staging_dir)?
        .metadata()?
        .dev();
    let mut target_device = None;
    let mut first_parent = None;
    for path in paths {
        let target = Path::new("/").join(path);
        let parent = target
            .parent()
            .context("original restore target has no parent")?;
        ensure_no_symlink_components(parent)?;
        let device = open_directory_no_follow(parent)?.metadata()?.dev();
        ensure!(
            target_device.is_none_or(|expected| expected == device),
            "one original-path restore cannot span multiple filesystems"
        );
        target_device = Some(device);
        first_parent.get_or_insert_with(|| parent.to_path_buf());
    }
    let target_device = target_device.context("original restore has no target filesystem")?;
    if target_device == configured_device {
        return Ok(config.restore.staging_dir.clone());
    }

    let mut filesystem_root = first_parent.context("original restore has no target parent")?;
    while let Some(parent) = filesystem_root.parent() {
        if open_directory_no_follow(parent)?.metadata()?.dev() != target_device {
            break;
        }
        filesystem_root = parent.to_path_buf();
    }
    let root_metadata = fs::symlink_metadata(&filesystem_root)?;
    ensure!(
        root_metadata.is_dir()
            && !root_metadata.file_type().is_symlink()
            && root_metadata.uid() == 0
            && root_metadata.mode() & 0o022 == 0,
        "target filesystem root is not a root-owned non-writable directory: {}",
        filesystem_root.display()
    );
    let fallback = filesystem_root.join(".boxup-restore").join(&config.host.id);
    for path in paths {
        let target = Path::new("/").join(path);
        ensure!(
            target != fallback && !target.starts_with(&fallback) && !fallback.starts_with(&target),
            "original restore target overlaps fallback staging: {}",
            target.display()
        );
    }
    create_directory_all_no_follow(&fallback, Mode::from_bits_truncate(0o700))?;
    let metadata = fs::symlink_metadata(&fallback)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.mode() & 0o077 == 0,
        "fallback restore staging is not root-owned mode 0700: {}",
        fallback.display()
    );
    Ok(fallback)
}

pub fn normalize_paths(config: &Config, inputs: &[String]) -> Result<Vec<String>> {
    config.validate()?;
    ensure!(!inputs.is_empty(), "at least one restore path is required");
    ensure!(inputs.len() <= 10_000, "too many restore path selections");
    let mut normalized = Vec::with_capacity(inputs.len());
    for input in inputs {
        ensure!(
            !input.chars().any(char::is_control),
            "restore path contains control characters"
        );
        let archive_path = input.strip_prefix('/').unwrap_or(input);
        ensure!(
            !archive_path.is_empty(),
            "restoring archive root is not allowed"
        );
        validate_archive_path(archive_path)
            .with_context(|| format!("invalid restore path {input:?}"))?;
        let relative = archive_path.to_owned();
        ensure_not_denied(config, &relative, true)?;
        if !normalized.contains(&relative) {
            normalized.push(relative);
        }
    }
    Ok(normalized)
}

pub fn validate_mountpoint(target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(target)
        .with_context(|| format!("mountpoint does not exist: {}", target.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "mountpoint may not be a symlink"
    );
    ensure!(metadata.is_dir(), "mountpoint must be a directory");
    ensure!(
        fs::read_dir(target)?.next().is_none(),
        "mountpoint must be empty"
    );
    ensure!(
        Path::new("/usr/bin/fusermount3").exists() || Path::new("/usr/bin/fusermount").exists(),
        "FUSE userspace support is missing; install pyfuse3 or llfuse and fusermount"
    );
    let fuse_module = std::process::Command::new("/usr/bin/python3")
        .args([
            "-c",
            "import importlib.util,sys; sys.exit(0 if importlib.util.find_spec('pyfuse3') or importlib.util.find_spec('llfuse') else 1)",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    ensure!(
        fuse_module.is_ok_and(|status| status.success()),
        "Borg mount requires the pyfuse3 or llfuse Python module"
    );
    Ok(())
}

async fn validate_live_selection<B: Backend + ?Sized>(
    backend: &B,
    config: &Config,
    snapshot: &str,
    paths: &[String],
) -> Result<RestorePlan> {
    validate_live_selection_with_progress(backend, config, snapshot, paths, |_| {}).await
}

async fn validate_live_selection_with_progress<B, F>(
    backend: &B,
    config: &Config,
    snapshot: &str,
    paths: &[String],
    mut progress: F,
) -> Result<RestorePlan>
where
    B: Backend + ?Sized,
    F: FnMut(u64),
{
    let snapshots = backend.list_snapshots().await?;
    let mut matching = snapshots
        .into_iter()
        .filter(|candidate| candidate.name == snapshot);
    let snapshot = matching
        .next()
        .context("snapshot does not exist in the live repository")?;
    ensure!(
        matching.next().is_none(),
        "live repository returned a duplicate snapshot name"
    );
    validate_archive_id(&snapshot.id)?;
    let mut stream = backend.list_files_selected(&snapshot.name, paths).await?;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut scanned = 0_u64;
    let mut found = vec![false; paths.len()];
    let mut manifest = BTreeMap::new();
    while let Some(item) = stream.try_next().await? {
        scanned = scanned
            .checked_add(1)
            .context("archive scan count overflow")?;
        if scanned == 1 || scanned % 250 == 0 {
            progress(scanned);
        }
        validate_archive_path(&item.path)
            .with_context(|| format!("live archive contains an unsafe path: {:?}", item.path))?;
        let selected = paths
            .iter()
            .enumerate()
            .filter_map(|(position, path)| {
                archive_path_matches(&item.path, path).then_some(position)
            })
            .collect::<Vec<_>>();
        let ancestor = paths
            .iter()
            .any(|path| item.path != *path && archive_path_matches(path, &item.path));
        if !selected.is_empty() || ancestor {
            for position in selected {
                found[position] = true;
            }
            if ancestor {
                ensure!(
                    item.kind == FileType::Directory,
                    "restore path has a non-directory archive ancestor: {}",
                    item.path
                );
            }
            ensure!(
                !item.kind.is_special(),
                "restore contains unsupported special file: {}",
                item.path
            );
            files = files
                .checked_add(1)
                .context("restore file count overflow")?;
            if item.kind == FileType::File {
                bytes = bytes
                    .checked_add(item.size)
                    .context("restore byte count overflow")?;
            }
            ensure!(
                files <= config.restore.max_files,
                "restore exceeds max_files"
            );
            ensure!(
                bytes <= config.restore.max_bytes,
                "restore exceeds max_bytes"
            );
            ensure!(
                manifest.insert(item.path.clone(), item).is_none(),
                "live archive contains a duplicate path"
            );
        }
    }
    progress(scanned);
    for (path, exists) in paths.iter().zip(found) {
        ensure!(exists, "selected path is absent from live snapshot: {path}");
    }
    add_synthetic_ancestors(paths, &mut manifest, &mut files, config.restore.max_files)?;
    Ok(RestorePlan {
        snapshot: snapshot.name,
        snapshot_id: snapshot.id,
        paths: paths.to_vec(),
        files,
        bytes,
        manifest,
    })
}

fn add_synthetic_ancestors(
    paths: &[String],
    manifest: &mut BTreeMap<String, ArchiveItem>,
    files: &mut u64,
    max_files: u64,
) -> Result<()> {
    for selected in paths {
        let mut ancestor = Path::new(selected).parent();
        while let Some(path) = ancestor.filter(|path| !path.as_os_str().is_empty()) {
            let path = path
                .to_str()
                .context("restore path ancestor is not UTF-8")?;
            if !manifest.contains_key(path) {
                *files = files
                    .checked_add(1)
                    .context("restore file count overflow")?;
                ensure!(*files <= max_files, "restore exceeds max_files");
                manifest.insert(
                    path.to_owned(),
                    ArchiveItem {
                        path: path.to_owned(),
                        kind: FileType::Directory,
                        size: 0,
                        mtime: None,
                        mode: None,
                        uid: None,
                        gid: None,
                        user: None,
                        group: None,
                        link_target: None,
                        health: None,
                    },
                );
            }
            ancestor = path.rsplit_once('/').map(|(parent, _)| Path::new(parent));
        }
    }
    Ok(())
}

async fn revalidate_snapshot<B: Backend + ?Sized>(backend: &B, plan: &RestorePlan) -> Result<()> {
    let snapshots = backend.list_snapshots().await?;
    let matching: Vec<_> = snapshots
        .iter()
        .filter(|snapshot| snapshot.name == plan.snapshot)
        .collect();
    ensure!(
        matching.len() == 1 && matching[0].id == plan.snapshot_id,
        "snapshot identity changed during restore"
    );
    Ok(())
}

fn ensure_not_denied(config: &Config, relative: &str, allow_inventory: bool) -> Result<()> {
    let absolute = Path::new("/").join(relative);
    let inventory = config.backup.state_dir.join("inventory");
    let mut denied: Vec<&Path> = config
        .restore
        .denied_paths
        .iter()
        .map(PathBuf::as_path)
        .collect();
    denied.extend([
        config.repository.passphrase_file.as_path(),
        config.repository.ssh_key.as_path(),
        config.repository.known_hosts.as_path(),
        config.backup.state_dir.as_path(),
        config.backup.cache_dir.as_path(),
        config.index.path.as_path(),
        config.restore.staging_dir.as_path(),
    ]);
    if let Some(path) = &config.source_path {
        denied.push(path);
    }
    if let Some(path) = &config.repository.maintenance_ssh_key {
        denied.push(path);
    }
    if let Some(path) = &config.notifications.discord_webhook_file {
        denied.push(path);
    }
    for path in denied {
        if allow_inventory
            && path == config.backup.state_dir
            && (absolute == inventory || absolute.starts_with(&inventory))
        {
            continue;
        }
        if absolute == path || absolute.starts_with(path) || path.starts_with(&absolute) {
            bail!(
                "restore selection overlaps protected path {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn ensure_target_not_denied(config: &Config, target: &Path) -> Result<()> {
    let relative = target
        .strip_prefix("/")?
        .to_str()
        .context("restore target is not UTF-8")?;
    ensure_not_denied(config, relative, false)?;
    if let Some(path) = &config.docker.staging_dir {
        ensure!(
            target != path && !target.starts_with(path) && !path.starts_with(target),
            "restore target overlaps Docker staging path {}",
            path.display()
        );
    }
    Ok(())
}

fn archive_path_matches(candidate: &str, selected: &str) -> bool {
    candidate == selected
        || candidate
            .strip_prefix(selected)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

fn validate_new_target(target: &Path) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "restore target may not be a symlink"
            );
            ensure!(
                metadata.is_dir(),
                "existing restore target must be a directory"
            );
            ensure!(
                fs::read_dir(target)?.next().is_none(),
                "existing restore target must be empty"
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_no_symlink_components(path: &Path) -> Result<()> {
    validate_absolute(path)?;
    let mut current = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                current.push(value);
                let metadata = fs::symlink_metadata(&current)?;
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "path traverses symlink: {}",
                    current.display()
                );
            }
            _ => bail!("path contains unsupported components: {}", path.display()),
        }
    }
    Ok(())
}

fn create_directory_all_no_follow(path: &Path, mode: Mode) -> Result<()> {
    validate_absolute(path)?;
    let mut directory = File::open("/")?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let child = match openat(&directory, name, flags, Mode::empty()) {
            Ok(child) => child,
            Err(Errno::ENOENT) => {
                match mkdirat(&directory, name, mode) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                openat(&directory, name, flags, Mode::empty()).with_context(|| {
                    format!(
                        "refusing to follow directory component in {}",
                        path.display()
                    )
                })?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "refusing to follow directory component in {}",
                        path.display()
                    )
                });
            }
        };
        directory = File::from(child);
    }
    Ok(())
}

fn verify_extracted_manifest(root: &Path, plan: &RestorePlan, config: &Config) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut observed = BTreeMap::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)?;
            let kind = metadata.file_type();
            ensure!(
                kind.is_file() || kind.is_dir() || kind.is_symlink(),
                "extracted tree contains a special file: {}",
                entry_path.display()
            );
            files = files
                .checked_add(1)
                .context("restore file count overflow")?;
            if kind.is_file() {
                bytes = bytes
                    .checked_add(metadata.len())
                    .context("restore byte count overflow")?;
            }
            ensure!(
                files <= config.restore.max_files && bytes <= config.restore.max_bytes,
                "extracted tree exceeds restore limits"
            );
            let relative = entry_path.strip_prefix(root)?;
            let relative = relative
                .to_str()
                .context("extracted path is not UTF-8")?
                .to_owned();
            let actual_kind = if kind.is_file() {
                FileType::File
            } else if kind.is_dir() {
                FileType::Directory
            } else {
                FileType::Symlink
            };
            let link_target = if kind.is_symlink() {
                Some(
                    fs::read_link(&entry_path)?
                        .to_str()
                        .context("extracted symlink target is not UTF-8")?
                        .to_owned(),
                )
            } else {
                None
            };
            ensure!(
                observed
                    .insert(relative, (actual_kind, metadata.len(), link_target))
                    .is_none(),
                "extracted tree contains a duplicate path"
            );
            if kind.is_dir() {
                pending.push(entry_path);
            }
        }
    }
    ensure!(
        observed.len() == plan.manifest.len(),
        "extracted manifest entry count differs from validated plan"
    );
    ensure!(
        files == plan.files,
        "extracted file count differs from validated plan"
    );
    ensure!(
        bytes == plan.bytes,
        "extracted byte count differs from validated plan"
    );
    for (path, expected) in &plan.manifest {
        let (kind, size, link_target) = observed
            .get(path)
            .with_context(|| format!("extracted manifest is missing {path}"))?;
        ensure!(kind == &expected.kind, "extracted type differs for {path}");
        if expected.kind == FileType::File {
            ensure!(*size == expected.size, "extracted size differs for {path}");
        }
        if expected.kind == FileType::Symlink {
            ensure!(
                link_target.as_deref() == expected.link_target.as_deref(),
                "extracted symlink target differs for {path}"
            );
        }
    }
    Ok(())
}

fn validate_restore_target(target: &Path) -> Result<()> {
    validate_absolute(target)?;
    ensure!(
        target != Path::new("/"),
        "normal restore target cannot be /"
    );
    Ok(())
}

fn publish_directory(staging: &Path, target: &Path) -> Result<()> {
    let target_parent = target.parent().context("restore target has no parent")?;
    ensure_no_symlink_components(target_parent)?;
    match fs::symlink_metadata(target) {
        Ok(_) => {
            validate_new_target(target)?;
            fs::remove_dir(target).context("failed to remove the existing empty restore target")?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let staging_parent = staging.parent().context("restore staging has no parent")?;
    let staging_name = staging
        .file_name()
        .context("restore staging has no file name")?;
    let target_name = target
        .file_name()
        .context("restore target has no file name")?;
    let staging_parent = open_directory_no_follow(staging_parent)?;
    let target_parent = open_directory_no_follow(target_parent)?;
    renameat2(
        &staging_parent,
        staging_name,
        &target_parent,
        target_name,
        RenameFlags::RENAME_NOREPLACE,
    )
    .context("atomic no-replace restore publication failed")?;
    Ok(())
}

fn publish_original_path(source: &Path, target: &Path) -> Result<()> {
    let source_parent_path = source.parent().context("staged path has no parent")?;
    let target_parent_path = target.parent().context("target path has no parent")?;
    ensure_no_symlink_components(target_parent_path)?;
    let source_parent = open_directory_no_follow(source_parent_path)?;
    let target_parent = open_directory_no_follow(target_parent_path)?;
    let source_name = source.file_name().context("staged path has no file name")?;
    let target_name = target.file_name().context("target path has no file name")?;
    match fs::symlink_metadata(target) {
        Ok(_) => renameat2(
            &source_parent,
            source_name,
            &target_parent,
            target_name,
            RenameFlags::RENAME_EXCHANGE,
        )
        .context("atomic original-path replacement failed")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => renameat2(
            &source_parent,
            source_name,
            &target_parent,
            target_name,
            RenameFlags::RENAME_NOREPLACE,
        )
        .context("atomic original-path publication failed")?,
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn open_directory_no_follow(path: &Path) -> Result<File> {
    validate_absolute(path)?;
    let mut directory = File::open("/")?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            let child = openat(
                &directory,
                name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| {
                format!(
                    "refusing to follow directory component in {}",
                    path.display()
                )
            })?;
            directory = File::from(child);
        }
    }
    Ok(directory)
}

fn validate_archive_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "archive path is empty");
    ensure!(!path.starts_with('/'), "archive path must be relative");
    ensure!(
        !path.chars().any(char::is_control),
        "archive path contains control characters"
    );
    ensure!(
        path.split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "archive path is not normalized"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "archive path contains unsupported components"
    );
    Ok(())
}

fn validate_archive_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "live snapshot has an invalid immutable archive ID"
    );
    Ok(())
}

fn merge_without_following(source: &Path, destination: &Path) -> Result<()> {
    let destination = File::open(destination)?;
    merge_into_directory(source, &destination)
}

fn merge_into_directory(source: &Path, destination: &File) -> Result<()> {
    let source_directory = File::open(source)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.is_dir() {
            match renameat(
                &source_directory,
                entry.file_name().as_os_str(),
                destination,
                entry.file_name().as_os_str(),
            ) {
                Ok(()) => continue,
                Err(Errno::ENOTEMPTY | Errno::EEXIST) => {
                    let child = openat(
                        destination,
                        entry.file_name().as_os_str(),
                        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                        Mode::empty(),
                    )
                    .context("refusing to traverse a destination symlink or non-directory")?;
                    let child = File::from(child);
                    merge_into_directory(&source_path, &child)?;
                    fchown(
                        &child,
                        Some(Uid::from_raw(metadata.uid())),
                        Some(Gid::from_raw(metadata.gid())),
                    )?;
                    fchmod(&child, Mode::from_bits_truncate(metadata.mode()))?;
                    futimens(
                        &child,
                        &TimeSpec::new(metadata.atime(), metadata.atime_nsec()),
                        &TimeSpec::new(metadata.mtime(), metadata.mtime_nsec()),
                    )?;
                    fs::remove_dir(&source_path)?;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            renameat(
                &source_directory,
                entry.file_name().as_os_str(),
                destination,
                entry.file_name().as_os_str(),
            )
            .context("failed to publish restore entry without following links")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use futures::stream;
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::backend::{DiffStream, FileStream};
    use crate::config::*;
    use crate::domain::{CreateRequest, DiffEntry, RepositoryIdentity, Snapshot};

    fn config(root: &Path) -> Config {
        Config {
            source_path: None,
            version: 1,
            host: HostConfig { id: "test".into() },
            repository: RepositoryConfig {
                location: root.join("repo").display().to_string(),
                passphrase_file: root.join("secret"),
                ssh_key: root.join("key"),
                maintenance_ssh_key: None,
                known_hosts: root.join("known_hosts"),
                ssh_port: 22,
                borg_path: "/usr/bin/borg".into(),
                remote_path: "borg-1.4".into(),
                lock_wait_seconds: 1,
            },
            backup: BackupConfig {
                sources: vec!["/home".into()],
                excludes: vec![],
                one_file_system: true,
                exclude_caches: true,
                compression: "lz4".into(),
                upload_rate_kib: None,
                state_dir: root.join("state"),
                cache_dir: root.join("cache"),
            },
            retention: RetentionConfig {
                keep_daily: 1,
                keep_weekly: 1,
                keep_monthly: 1,
                require_backup_within_hours: 24,
            },
            restore: RestoreConfig {
                staging_dir: root.join("staging"),
                denied_paths: vec!["/etc/boxup".into()],
                max_files: 10,
                max_bytes: 1024,
            },
            index: IndexConfig {
                path: root.join("index.sqlite3"),
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
    fn rejects_traversal_and_root() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        assert!(normalize_paths(&config, &["../etc/shadow".into()]).is_err());
        assert!(normalize_paths(&config, &["/".into()]).is_err());
    }

    #[test]
    fn wildcard_is_preserved_literally() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        assert_eq!(
            normalize_paths(&config, &["/home/a*".into()]).unwrap(),
            ["home/a*"]
        );
        assert!(archive_path_matches("home/a*", "home/a*"));
        assert!(!archive_path_matches("home/alice", "home/a*"));
    }

    #[test]
    fn selector_prefixes_are_preserved_as_literal_names() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        let inputs = ["re:item", "sh:item", "fm:item", "pp:item", "pf:item"].map(str::to_owned);
        assert_eq!(normalize_paths(&config, &inputs).unwrap(), inputs);
    }

    #[test]
    fn rejects_non_normal_restore_selections() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        for input in [
            "home/./user",
            "home/../user",
            "/home//user",
            "home/user/",
            "//home/user",
            "home/new\nline",
        ] {
            assert!(normalize_paths(&config, &[input.into()]).is_err());
        }
    }

    #[test]
    fn restore_path_normalization_rejects_invalid_configured_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut invalid_config = config(temp.path());
        invalid_config.backup.sources = vec!["/home/./user".into()];
        assert!(normalize_paths(&invalid_config, &["home/user".into()]).is_err());

        let mut invalid_config = config(temp.path());
        invalid_config.restore.denied_paths = vec!["/etc/../secret".into()];
        assert!(normalize_paths(&invalid_config, &["home/user".into()]).is_err());
    }

    #[test]
    fn rejects_credential_and_loaded_config_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = config(temp.path());
        config.source_path = Some("/home/user/.config/boxup/config.toml".into());
        config.repository.maintenance_ssh_key = Some("/etc/boxup/maintenance_key".into());
        config.notifications.discord_webhook_file = Some("/run/credentials/webhook".into());

        assert!(normalize_paths(&config, &["/home/user/.config".into()]).is_err());
        assert!(normalize_paths(&config, &["/etc/boxup/maintenance_key".into()]).is_err());
        assert!(normalize_paths(&config, &["/run/credentials".into()]).is_err());
    }

    #[test]
    fn rejects_existing_nonempty_destination() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("occupied"), "x").unwrap();
        assert!(validate_new_target(temp.path()).is_err());
    }

    #[test]
    fn rejects_non_normal_target_components() {
        assert!(validate_restore_target(Path::new("/tmp/./restore")).is_err());
        assert!(validate_restore_target(Path::new("/tmp/../restore")).is_err());
        assert!(validate_restore_target(Path::new("/tmp//restore")).is_err());
        assert!(validate_restore_target(Path::new("/tmp/restore\nname")).is_err());
    }

    #[test]
    fn allows_selecting_archived_docker_staging_for_external_restore() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = config(temp.path());
        let docker_staging = temp.path().join("docker-staging");
        config.docker.enabled = true;
        config.docker.staging_dir = Some(docker_staging.clone());
        let selected = docker_staging
            .strip_prefix("/")
            .unwrap()
            .display()
            .to_string();
        assert_eq!(
            normalize_paths(&config, std::slice::from_ref(&selected)).unwrap(),
            [selected]
        );
        assert!(ensure_target_not_denied(&config, &docker_staging).is_err());
        assert!(ensure_target_not_denied(&config, &temp.path().join("external-restore")).is_ok());
    }

    #[test]
    fn allows_selecting_archived_inventory_only_for_external_restore() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        let inventory = config.backup.state_dir.join("inventory/archive.json");
        let selected = inventory.strip_prefix("/").unwrap().display().to_string();

        assert_eq!(
            normalize_paths(&config, std::slice::from_ref(&selected)).unwrap(),
            [selected]
        );
        assert!(
            normalize_paths(
                &config,
                &[config
                    .backup
                    .state_dir
                    .join("last-success.json")
                    .strip_prefix("/")
                    .unwrap()
                    .display()
                    .to_string()]
            )
            .is_err()
        );
        assert!(ensure_target_not_denied(&config, &inventory).is_err());
        assert!(ensure_target_not_denied(&config, &temp.path().join("external-restore")).is_ok());
    }

    #[test]
    fn rejects_target_symlink_hazard() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(validate_new_target(&link).is_err());
        assert!(ensure_no_symlink_components(&link).is_err());
    }

    #[test]
    fn directory_creation_never_follows_a_symlink_component() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let link = temp.path().join("link");
        symlink(&outside, &link).unwrap();

        assert!(
            create_directory_all_no_follow(&link.join("created"), Mode::from_bits_truncate(0o700))
                .is_err()
        );
        assert!(!outside.join("created").exists());
    }

    #[test]
    fn publishes_into_an_existing_empty_destination() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let target = temp.path().join("target");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(staging.join("restored"), "data").unwrap();

        publish_directory(&staging, &target).unwrap();

        assert_eq!(fs::read_to_string(target.join("restored")).unwrap(), "data");
        assert!(!staging.exists());
    }

    #[test]
    fn original_path_publication_exactly_replaces_existing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("staged/hypr");
        let target = temp.path().join("home/hypr");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(staged.join("old-config"), "snapshot").unwrap();
        fs::write(target.join("new-only"), "current").unwrap();

        publish_original_path(&staged, &target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("old-config")).unwrap(),
            "snapshot"
        );
        assert!(!target.join("new-only").exists());
        assert_eq!(
            fs::read_to_string(staged.join("new-only")).unwrap(),
            "current"
        );
    }

    #[test]
    fn parent_restore_selection_supersedes_selected_descendants() {
        assert_eq!(
            minimal_restore_paths(vec![
                "home/la/.config/hypr/hyprland.conf".into(),
                "home/la/.config/hypr".into(),
                "home/la/Documents".into(),
            ]),
            ["home/la/.config/hypr", "home/la/Documents"]
        );
    }

    #[test]
    fn missing_original_parents_are_published_as_one_directory() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("Bewerbungsunterlagen");
        let paths = [missing.join("CV"), missing.join("Minijobs")]
            .map(|path| path.strip_prefix("/").unwrap().display().to_string());

        let publication = original_publication_paths(&paths).unwrap();

        assert_eq!(
            publication,
            [missing.strip_prefix("/").unwrap().display().to_string()]
        );
    }

    #[test]
    fn publication_never_replaces_an_occupied_destination() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let target = temp.path().join("target");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(staging.join("restored"), "new").unwrap();
        fs::write(target.join("existing"), "old").unwrap();

        assert!(publish_directory(&staging, &target).is_err());
        assert_eq!(fs::read_to_string(target.join("existing")).unwrap(), "old");
        assert_eq!(fs::read_to_string(staging.join("restored")).unwrap(), "new");
    }

    #[test]
    fn overwrite_merge_refuses_symlink_directory_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let outside = temp.path().join("outside");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("existing")).unwrap();
        fs::write(source.join("existing/restored"), "new").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("untouched"), "old").unwrap();
        symlink(&outside, destination.join("existing")).unwrap();

        let result = merge_without_following(&source, &destination);
        assert_eq!(
            fs::read_to_string(outside.join("untouched")).unwrap(),
            "old"
        );
        assert!(!outside.join("restored").exists());
        if result.is_ok() {
            assert_eq!(
                fs::read_to_string(destination.join("existing/restored")).unwrap(),
                "new"
            );
        }
    }

    #[test]
    fn rejects_special_extracted_types() {
        let temp = tempfile::tempdir().unwrap();
        let fifo = temp.path().join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `path` is a valid NUL-terminated path and mode has no invalid bits.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let config = config(temp.path());
        let plan = RestorePlan {
            snapshot: "test".into(),
            snapshot_id: "a".repeat(64),
            paths: vec!["fifo".into()],
            files: 1,
            bytes: 0,
            manifest: BTreeMap::new(),
        };
        assert!(verify_extracted_manifest(temp.path(), &plan, &config).is_err());
    }

    #[test]
    fn extracted_manifest_must_match_plan_exactly() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("etc")).unwrap();
        fs::write(temp.path().join("etc/hosts"), "data").unwrap();
        let config = config(temp.path());
        let mut manifest = BTreeMap::new();
        manifest.insert("etc".into(), archive_item("etc", FileType::Directory, 0));
        manifest.insert(
            "etc/hosts".into(),
            archive_item("etc/hosts", FileType::File, 4),
        );
        let plan = RestorePlan {
            snapshot: "test".into(),
            snapshot_id: "a".repeat(64),
            paths: vec!["etc/hosts".into()],
            files: 2,
            bytes: 4,
            manifest,
        };
        verify_extracted_manifest(temp.path(), &plan, &config).unwrap();
        fs::write(temp.path().join("extra"), "unexpected").unwrap();
        assert!(verify_extracted_manifest(temp.path(), &plan, &config).is_err());
    }

    #[test]
    fn extracted_manifest_includes_deterministic_synthetic_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("var/lib")).unwrap();
        fs::write(temp.path().join("var/lib/inventory.json"), "data").unwrap();
        let config = config(temp.path());
        let mut manifest = BTreeMap::new();
        manifest.insert(
            "var/lib".into(),
            archive_item("var/lib", FileType::Directory, 0),
        );
        manifest.insert(
            "var/lib/inventory.json".into(),
            archive_item("var/lib/inventory.json", FileType::File, 4),
        );
        let mut files = 2;
        add_synthetic_ancestors(
            &["var/lib/inventory.json".into()],
            &mut manifest,
            &mut files,
            config.restore.max_files,
        )
        .unwrap();
        let plan = RestorePlan {
            snapshot: "test".into(),
            snapshot_id: "a".repeat(64),
            paths: vec!["var/lib/inventory.json".into()],
            files,
            bytes: 4,
            manifest,
        };

        assert_eq!(plan.files, 3);
        assert_eq!(plan.manifest["var"].kind, FileType::Directory);
        verify_extracted_manifest(temp.path(), &plan, &config).unwrap();
    }

    fn archive_item(path: &str, kind: FileType, size: u64) -> ArchiveItem {
        ArchiveItem {
            path: path.into(),
            kind,
            size,
            mtime: None,
            mode: None,
            uid: None,
            gid: None,
            user: None,
            group: None,
            link_target: None,
            health: None,
        }
    }

    struct RestoreBackend {
        state_dir: PathBuf,
        snapshot_calls: AtomicUsize,
        lock_observed: Arc<AtomicBool>,
        change_identity_at: Option<usize>,
    }

    #[async_trait]
    impl Backend for RestoreBackend {
        async fn preflight(&self) -> Result<()> {
            Ok(())
        }

        async fn repository_exists(&self) -> Result<bool> {
            Ok(true)
        }

        async fn init_repository(&self) -> Result<()> {
            bail!("not used")
        }

        async fn repository_identity(&self) -> Result<RepositoryIdentity> {
            Ok(RepositoryIdentity {
                id: "f".repeat(64),
                location: "/test".into(),
            })
        }

        async fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
            let call = self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
            let id = if self
                .change_identity_at
                .is_some_and(|change_identity_at| call >= change_identity_at)
            {
                "b".repeat(64)
            } else {
                "a".repeat(64)
            };
            Ok(vec![Snapshot {
                id,
                name: "test-archive".into(),
                start: chrono::DateTime::UNIX_EPOCH,
                end: None,
                hostname: None,
                username: None,
            }])
        }

        async fn list_files(&self, _snapshot: &str, _path: Option<&str>) -> Result<FileStream> {
            Ok(Box::pin(stream::iter([
                Ok(archive_item("etc", FileType::Directory, 0)),
                Ok(archive_item("etc/hosts", FileType::File, 4)),
            ])))
        }

        async fn create(&self, _request: &CreateRequest) -> Result<Snapshot> {
            bail!("not used")
        }

        async fn extract(
            &self,
            _snapshot: &str,
            paths: &[String],
            destination: &Path,
        ) -> Result<()> {
            ensure!(paths == ["etc/hosts"], "unexpected restore selection");
            self.lock_observed.store(
                LocalLock::acquire(&self.state_dir, LockMode::Exclusive).is_err(),
                Ordering::SeqCst,
            );
            fs::create_dir(destination.join("etc"))?;
            fs::write(destination.join("etc/hosts"), "data")?;
            Ok(())
        }

        async fn mount(&self, _snapshot: &str, _target: &Path) -> Result<()> {
            bail!("not used")
        }

        async fn umount(&self, _target: &Path) -> Result<()> {
            bail!("not used")
        }

        async fn diff(&self, _a: &str, _b: &str, _path: Option<&str>) -> Result<DiffStream> {
            Ok(Box::pin(stream::empty::<Result<DiffEntry>>()))
        }

        async fn prune(
            &self,
            _archive_prefix: &str,
            _keep: (u32, u32, u32),
            _dry_run: bool,
        ) -> Result<()> {
            bail!("not used")
        }

        async fn compact(&self) -> Result<()> {
            bail!("not used")
        }

        async fn check(&self, _verify_data: bool) -> Result<()> {
            bail!("not used")
        }

        async fn key_export(&self, _destination: &Path) -> Result<()> {
            bail!("not used")
        }
    }

    #[tokio::test]
    async fn restore_holds_lock_and_revalidates_archive_id_before_publish() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        let lock_observed = Arc::new(AtomicBool::new(false));
        let backend = RestoreBackend {
            state_dir: config.backup.state_dir.clone(),
            snapshot_calls: AtomicUsize::new(0),
            lock_observed: Arc::clone(&lock_observed),
            change_identity_at: Some(2),
        };
        let target = temp.path().join("published");

        let error = restore(
            &backend,
            &config,
            "test-archive",
            &["etc/hosts".into()],
            &target,
        )
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("snapshot identity changed"));
        assert!(lock_observed.load(Ordering::SeqCst));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn restore_revalidates_archive_id_before_extraction() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        let lock_observed = Arc::new(AtomicBool::new(false));
        let backend = RestoreBackend {
            state_dir: config.backup.state_dir.clone(),
            snapshot_calls: AtomicUsize::new(0),
            lock_observed: Arc::clone(&lock_observed),
            change_identity_at: Some(1),
        };
        let target = temp.path().join("published");

        let error = restore(
            &backend,
            &config,
            "test-archive",
            &["etc/hosts".into()],
            &target,
        )
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("snapshot identity changed"));
        assert!(!lock_observed.load(Ordering::SeqCst));
        assert!(!target.exists());
    }
}
