use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Stdio;

use boxup::borg::{BorgExit, BorgRunner};
use boxup::config::*;
use boxup::domain::CreateRequest;
use boxup::index::Index;
use boxup::jobs::JobRunner;
use boxup::{Backend, BorgBackend};
use futures::TryStreamExt;

#[tokio::test]
async fn passes_secret_only_through_fd_and_builds_strict_environment() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    let passphrase = temp.path().join("passphrase");
    fs::write(&passphrase, "correct horse battery staple\n").unwrap();
    set_private(&passphrase);
    fs::write(
        &script,
        r#"#!/bin/sh
set -eu
secret=$(cat)
[ "$secret" = 'correct horse battery staple' ]
[ "${BORG_PASSPHRASE_FD:-}" = 0 ]
[ "${BORG_REMOTE_PATH:-}" = borg ]
[ "${BORG_EXIT_CODES:-}" = modern ]
[ "${TZ:-}" = UTC ]
[ "${BORG_REPO:-}" = /tmp/fake-repository ]
[ -z "${SSH_AUTH_SOCK:-}" ]
case "${1:-}" in
  --version)
    [ "${BORG_RSH:-}" = 'ssh -p 22 -i /etc/boxup/test_key -o UserKnownHostsFile=/etc/boxup/known_hosts -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes -o BatchMode=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=3' ]
    printf '%s\n' 'borg 1.4.1'
    ;;
  --maintenance-check)
    [ "${BORG_RSH:-}" = 'ssh -p 22 -i /etc/boxup/maintenance_key -o UserKnownHostsFile=/etc/boxup/known_hosts -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes -o BatchMode=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=3' ]
    printf '%s\n' 'maintenance key selected'
    ;;
  *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&script, permissions).unwrap();

    let runner = BorgRunner::new(
        RepositoryConfig {
            location: "/tmp/fake-repository".into(),
            passphrase_file: passphrase,
            ssh_key: "/etc/boxup/test_key".into(),
            maintenance_ssh_key: Some("/etc/boxup/maintenance_key".into()),
            known_hosts: "/etc/boxup/known_hosts".into(),
            ssh_port: 22,
            borg_path: script,
            remote_path: "borg".into(),
            lock_wait_seconds: 30,
        },
        PathBuf::from("/tmp/fake-cache"),
    );
    let output = runner.run(["--version"], None, false).await.unwrap();
    assert_eq!(output.exit, BorgExit::Success);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "borg 1.4.1"
    );
    let output = runner
        .run(["--maintenance-check"], None, true)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "maintenance key selected"
    );
}

#[tokio::test]
async fn passphrase_symlink_is_rejected_before_borg_starts() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real-passphrase");
    let link = temp.path().join("passphrase-link");
    fs::write(&real, "secret").unwrap();
    set_private(&real);
    symlink(&real, &link).unwrap();
    let script = temp.path().join("must-not-run");
    write_executable(&script, "#!/bin/sh\nexit 99\n");
    let mut config = fake_config(temp.path(), script);
    config.repository.passphrase_file = link;
    let runner = BorgRunner::new(config.repository, config.backup.cache_dir);
    assert!(runner.run(["--version"], None, false).await.is_err());
}

#[tokio::test]
async fn parses_fixture_streams_and_refreshes_index_incrementally() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    let source = r#"#!/bin/sh
set -eu
secret=$(cat)
[ "$secret" = test-passphrase ]
[ "${BORG_REMOTE_PATH:-}" = borg-1.4 ]
[ "${TZ:-}" = UTC ]
case "${1:-}:${2:-}" in
  --version:) printf '%s\n' 'borg 1.4.1' ;;
  info:--json) printf '%s\n' '__REPOSITORY_LIST__' ;;
  list:--json) printf '%s\n' '__REPOSITORY_LIST__' ;;
  list:--json-lines) printf '%s\n' '__ARCHIVE_LIST__' ;;
  diff:--json-lines)
    [ "${3:-}" = '::test-20260722T040000Z-1' ]
    [ "${4:-}" = 'test-20260722T040000Z-2' ]
    printf '%s\n' '__DIFF__'
    ;;
  *) printf '%s\n' 'unexpected fake Borg arguments' >&2; exit 2 ;;
esac
"#
    .replace(
        "__REPOSITORY_LIST__",
        include_str!("fixtures/repository-list.json"),
    )
    .replace(
        "__ARCHIVE_LIST__",
        include_str!("fixtures/archive-list.jsonl"),
    )
    .replace("__DIFF__", include_str!("fixtures/diff.jsonl"));
    write_executable(&script, &source);
    let config = fake_config(temp.path(), script);
    config.validate().unwrap();
    let backend = BorgBackend::new(&config);
    backend.preflight().await.unwrap();
    let snapshots = backend.list_snapshots().await.unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].start.to_rfc3339(),
        "2026-07-22T04:00:00.123456+00:00"
    );

    let mut files = backend
        .list_files(&snapshots[0].name, Some("etc/hosts"))
        .await
        .unwrap();
    assert_eq!(files.try_next().await.unwrap().unwrap().path, "etc/hosts");
    assert!(files.try_next().await.unwrap().is_none());
    let all_files = backend
        .list_files(&snapshots[0].name, None)
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(all_files[0].user.as_deref(), Some("0"));
    assert_eq!(all_files[0].group.as_deref(), Some("0"));
    assert_eq!((all_files[0].uid, all_files[0].gid), (Some(0), Some(0)));
    assert_eq!(all_files[2].user.as_deref(), Some("1000"));
    assert_eq!(all_files[2].group.as_deref(), Some("user"));
    assert_eq!(
        (all_files[2].uid, all_files[2].gid),
        (Some(1000), Some(1000))
    );

    let mut diff = backend
        .diff(&snapshots[0].name, "test-20260722T040000Z-2", Some("etc"))
        .await
        .unwrap();
    assert_eq!(diff.try_next().await.unwrap().unwrap().path, "etc/hosts");
    assert!(diff.try_next().await.unwrap().is_none());

    let index = Index::open(&config.index.path).unwrap();
    let first = index.refresh(&backend).await.unwrap();
    assert_eq!((first.archives_added, first.files_added), (1, 3));
    let second = index.refresh(&backend).await.unwrap();
    assert_eq!((second.archives_added, second.files_added), (0, 0));
    let status = index.status().unwrap();
    assert!(status.complete);
    assert_eq!(
        status.repository_id.as_deref(),
        Some("a".repeat(64).as_str())
    );
    assert!(
        index
            .is_usable("/tmp/fake-repository", std::time::Duration::from_secs(3600))
            .unwrap()
    );
    assert_eq!(index.search("hosts", true).unwrap().len(), 1);
}

#[tokio::test]
async fn warning_during_stream_rolls_back_index_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    let source = r#"#!/bin/sh
set -eu
cat >/dev/null
case "${1:-}:${2:-}" in
  list:--json) printf '%s\n' '__REPOSITORY_LIST__' ;;
  info:--json) printf '%s\n' '__REPOSITORY_LIST__' ;;
  list:--json-lines)
    printf '%s\n' '{"path":"etc/hosts","type":"file","size":128}'
    printf '%s\n' 'repository warning' >&2
    exit 100
    ;;
  *) exit 2 ;;
esac
"#
    .replace(
        "__REPOSITORY_LIST__",
        include_str!("fixtures/repository-list.json"),
    );
    write_executable(&script, &source);
    let config = fake_config(temp.path(), script);
    let backend = BorgBackend::new(&config);
    let index = Index::open(&config.index.path).unwrap();

    assert!(index.refresh(&backend).await.is_err());
    assert!(index.snapshots().unwrap().is_empty());
    assert!(!index.status().unwrap().complete);
}

#[tokio::test]
async fn repository_identity_change_rolls_back_index_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    let identity_calls = temp.path().join("identity-calls");
    let changed_identity =
        include_str!("fixtures/repository-list.json").replace(&"a".repeat(64), &"d".repeat(64));
    let source = r#"#!/bin/sh
set -eu
cat >/dev/null
case "${1:-}:${2:-}" in
  info:--json)
    if [ -e '__IDENTITY_CALLS__' ]; then
      printf '%s\n' '__CHANGED_REPOSITORY__'
    else
      : >'__IDENTITY_CALLS__'
      printf '%s\n' '__REPOSITORY_LIST__'
    fi
    ;;
  list:--json) printf '%s\n' '__REPOSITORY_LIST__' ;;
  list:--json-lines) printf '%s\n' '__ARCHIVE_LIST__' ;;
  *) exit 2 ;;
esac
"#
    .replace("__IDENTITY_CALLS__", &identity_calls.display().to_string())
    .replace("__CHANGED_REPOSITORY__", &changed_identity)
    .replace(
        "__REPOSITORY_LIST__",
        include_str!("fixtures/repository-list.json"),
    )
    .replace(
        "__ARCHIVE_LIST__",
        include_str!("fixtures/archive-list.jsonl"),
    );
    write_executable(&script, &source);
    let config = fake_config(temp.path(), script);
    let backend = BorgBackend::new(&config);
    let index = Index::open(&config.index.path).unwrap();

    let error = index.refresh(&backend).await.unwrap_err();
    assert!(format!("{error:#}").contains("identity changed"));
    assert!(index.snapshots().unwrap().is_empty());
    assert!(!index.status().unwrap().complete);
}

#[tokio::test]
async fn job_runner_parses_create_timestamp_and_stamps_archive_id() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    let source = r#"#!/bin/sh
set -eu
cat >/dev/null
case "${1:-}:${2:-}" in
  create:--json)
    archive=
    for argument in "$@"; do
      case "$argument" in ::*) archive=${argument#::} ;; esac
    done
    [ -n "$archive" ]
    printf '{"archive":{"id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","name":"%s","start":"2026-07-22T04:00:00.123456","end":"2026-07-22T04:01:00.654321","hostname":"test","username":"root"}}\n' "$archive"
    ;;
  info:--json) printf '%s\n' '__REPOSITORY_LIST__' ;;
  list:--json) printf '%s\n' '__REPOSITORY_LIST__' ;;
  list:--json-lines) printf '%s\n' '__ARCHIVE_LIST__' ;;
  *) exit 2 ;;
esac
"#
    .replace(
        "__REPOSITORY_LIST__",
        include_str!("fixtures/repository-list.json"),
    )
    .replace(
        "__ARCHIVE_LIST__",
        include_str!("fixtures/archive-list.jsonl"),
    );
    write_executable(&script, &source);
    let config = fake_config(temp.path(), script);
    fs::create_dir(temp.path().join("source")).unwrap();
    let backend = BorgBackend::new(&config);
    let index = Index::open(&config.index.path).unwrap();

    let snapshot = JobRunner::new(&config, &backend, &index)
        .backup()
        .await
        .unwrap();
    assert_eq!(snapshot.id, "c".repeat(64));
    assert_eq!(
        snapshot.start.to_rfc3339(),
        "2026-07-22T04:00:00.123456+00:00"
    );
    let stamp: serde_json::Value = serde_json::from_slice(
        &fs::read(config.backup.state_dir.join("last-success.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stamp["archive_id"], "c".repeat(64));
}

#[tokio::test]
async fn extract_uses_inclusive_prefix_patterns_and_key_export_never_overwrites() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    let source = r#"#!/bin/sh
set -eu
cat >/dev/null
case "${1:-}:${2:-}" in
  extract:--pattern)
    [ "$*" = 'extract --pattern + pp:home/literal --pattern + pp:home/a* --pattern + pp:re:literal --pattern + pp:sh:literal --pattern + pp:fm:literal --pattern + pp:pp:literal --pattern + pp:pf:literal --pattern - re:.* ::snapshot' ]
    ;;
  key:export)
    [ "$#" -eq 2 ]
    printf '%s\n' 'exported key'
    ;;
  *) exit 2 ;;
esac
"#;
    write_executable(&script, source);
    let config = fake_config(temp.path(), script);
    let backend = BorgBackend::new(&config);
    let destination = temp.path().join("destination");
    fs::create_dir(&destination).unwrap();
    backend
        .extract(
            "snapshot",
            &[
                "home/literal".into(),
                "home/a*".into(),
                "re:literal".into(),
                "sh:literal".into(),
                "fm:literal".into(),
                "pp:literal".into(),
                "pf:literal".into(),
            ],
            &destination,
        )
        .await
        .unwrap();

    let export = destination.join("repository.repokey");
    backend.key_export(&export).await.unwrap();
    assert_eq!(
        fs::metadata(&export).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(backend.key_export(&export).await.is_err());
}

#[tokio::test]
async fn key_export_losing_destination_race_does_not_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    let destination_dir = temp.path().join("destination");
    let destination = destination_dir.join("repository.repokey");
    fs::create_dir(&destination_dir).unwrap();
    let source = r#"#!/bin/sh
set -eu
cat >/dev/null
case "${1:-}:${2:-}" in
  key:export)
    [ "$#" -eq 2 ]
    printf '%s\n' 'exported key'
    printf '%s\n' 'competing file' >'__DESTINATION__'
    ;;
  *) exit 2 ;;
esac
"#
    .replace("__DESTINATION__", &destination.display().to_string());
    write_executable(&script, &source);
    let config = fake_config(temp.path(), script);
    let backend = BorgBackend::new(&config);

    assert!(backend.key_export(&destination).await.is_err());
    assert!(fs::read_to_string(&destination).unwrap() == "competing file\n");
    assert!(fs::read_dir(&destination_dir).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".boxup-key-")
    }));
}

#[tokio::test]
async fn optional_real_borg_uses_only_a_temporary_local_repository() {
    let borg_path = PathBuf::from("/usr/bin/borg");
    let version = std::process::Command::new(&borg_path)
        .arg("--version")
        .output();
    let Ok(version) = version else { return };
    if !version.status.success() || !String::from_utf8_lossy(&version.stdout).contains("borg 1.4") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repo");
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("fixture.txt"), "boxup fixture").unwrap();
    let mut config = fake_config(temp.path(), borg_path.clone());
    config.repository.location = repository.display().to_string();
    config.backup.sources = vec![source.clone()];
    config.validate().unwrap();
    let passphrase = fs::read(&config.repository.passphrase_file).unwrap();
    let mut init = std::process::Command::new(&borg_path)
        .args(["init", "--encryption=repokey-blake2"])
        .arg(&repository)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("BORG_PASSPHRASE_FD", "0")
        .env("BORG_BASE_DIR", temp.path().join("borg-init"))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    init.stdin.as_mut().unwrap().write_all(&passphrase).unwrap();
    let init = init.wait().unwrap();
    assert!(init.success());
    let backend = BorgBackend::new(&config);
    backend.preflight().await.unwrap();
    let first = backend
        .create(&CreateRequest {
            archive_name: "fixture-one".into(),
            sources: vec![source.clone()],
            excludes: vec![],
            one_file_system: true,
            exclude_caches: false,
            compression: "lz4".into(),
            upload_rate_kib: None,
        })
        .await
        .unwrap();
    assert_eq!(first.id.len(), 64);

    let raw_list = backend
        .runner()
        .run(["list", "--json"], None, false)
        .await
        .unwrap();
    let raw_list: serde_json::Value = serde_json::from_slice(&raw_list.stdout).unwrap();
    let raw_start = raw_list["archives"][0]["start"].as_str().unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(raw_start).is_err());
    let snapshots = backend.list_snapshots().await.unwrap();
    assert_eq!(snapshots[0].start.offset(), &chrono::Utc);

    let files = backend
        .list_files("fixture-one", None)
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let archived_file = files
        .iter()
        .find(|item| item.path.ends_with("/fixture.txt"))
        .unwrap();
    assert!(archived_file.user.is_some());
    let destination = temp.path().join("extract");
    fs::create_dir(&destination).unwrap();
    backend
        .extract(
            "fixture-one",
            std::slice::from_ref(&archived_file.path),
            &destination,
        )
        .await
        .unwrap();
    assert!(destination.join(&archived_file.path).is_file());

    fs::write(source.join("fixture.txt"), "changed fixture").unwrap();
    backend
        .create(&CreateRequest {
            archive_name: "fixture-two".into(),
            sources: vec![source],
            excludes: vec![],
            one_file_system: true,
            exclude_caches: false,
            compression: "lz4".into(),
            upload_rate_kib: None,
        })
        .await
        .unwrap();
    let differences = backend
        .diff("fixture-one", "fixture-two", None)
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert!(
        differences
            .iter()
            .any(|entry| entry.path == archived_file.path)
    );

    let exported_key = temp.path().join("repository.repokey");
    backend.key_export(&exported_key).await.unwrap();
    assert!(exported_key.is_file());
    assert_eq!(
        fs::metadata(exported_key).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

fn write_executable(path: &std::path::Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).unwrap();
}

fn fake_config(root: &std::path::Path, borg_path: PathBuf) -> Config {
    let passphrase = root.join("passphrase-fixture");
    fs::write(&passphrase, "test-passphrase\n").unwrap();
    set_private(&passphrase);
    Config {
        source_path: None,
        version: 1,
        host: HostConfig { id: "test".into() },
        repository: RepositoryConfig {
            location: "/tmp/fake-repository".into(),
            passphrase_file: passphrase,
            ssh_key: root.join("key"),
            maintenance_ssh_key: None,
            known_hosts: root.join("known_hosts"),
            ssh_port: 22,
            borg_path,
            remote_path: "borg-1.4".into(),
            lock_wait_seconds: 1,
        },
        backup: BackupConfig {
            sources: vec![root.join("source")],
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
            staging_dir: root.join("restore"),
            denied_paths: vec![],
            max_files: 100,
            max_bytes: 1_000_000,
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
            postgres_users: Default::default(),
            stop_services: vec![],
            service_paths: vec![],
            min_free_bytes: 1,
            docker_path: "/usr/bin/docker".into(),
            rsync_path: "/usr/bin/rsync".into(),
            systemctl_path: "/usr/bin/systemctl".into(),
        },
    }
}

fn set_private(path: &std::path::Path) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions).unwrap();
}
