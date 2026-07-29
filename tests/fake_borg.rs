use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

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
    printf '%s\n' '{"type":"archive_progress","finished":false,"nfiles":7,"original_size":4096,"compressed_size":2048,"deduplicated_size":1024,"path":"must/not/escape"}' >&2
    printf '{"archive":{"id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","name":"%s","start":"2026-07-22T04:00:00.123456","end":"2026-07-22T04:01:00.654321","hostname":"test","username":"root","stats":{"nfiles":7,"original_size":4096,"compressed_size":2048,"deduplicated_size":1024}}}\n' "$archive"
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

    let progress = Mutex::new(Vec::new());
    let snapshot = JobRunner::new(&config, &backend, &index)
        .backup_with_progress(|event| progress.lock().unwrap().push(event))
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
    let progress = progress.into_inner().unwrap();
    assert!(progress.iter().any(|event| {
        event.phase == boxup::domain::BackupPhase::CreatingArchive
            && event.files == 7
            && event.original_bytes == 4096
            && event.deduplicated_bytes == 1024
    }));
    assert_eq!(
        progress.last().unwrap().phase,
        boxup::domain::BackupPhase::Complete
    );
    let job = index.recent_jobs(1).unwrap().pop().unwrap();
    assert_eq!(job.archive_name.as_deref(), Some(snapshot.name.as_str()));
    assert_eq!(job.archive_id.as_deref(), Some(snapshot.id.as_str()));
    assert_eq!(
        (job.files, job.original_bytes, job.compressed_bytes),
        (7, 4096, 2048)
    );
}

#[tokio::test]
async fn create_file_changed_warning_completes_backup_with_note() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    let archive_name = temp.path().join("created-archive");
    write_executable(
        &script,
        &r#"#!/bin/sh
set -eu
cat >/dev/null
case "${1:-}:${2:-}" in
  create:--json)
    archive=
    for argument in "$@"; do
      case "$argument" in ::*) archive=${argument#::} ;; esac
    done
    printf '%s\n' "$archive" >'__ARCHIVE_NAME__'
    printf '%s\n' '{"type":"log_message","levelname":"WARNING","message":"file changed while we backed it up","msgid":"BackupRaceCondition"}' >&2
    printf '{"archive":{"id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","name":"%s","start":"2026-07-22T04:00:00Z"}}\n' "$archive"
    exit 100
    ;;
  info:--json) printf '%s\n' '__REPOSITORY_LIST__' ;;
  list:--json)
    archive=$(cat '__ARCHIVE_NAME__')
    printf '{"archives":[{"id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","name":"%s","start":"2026-07-22T04:00:00Z"}]}\n' "$archive"
    ;;
  list:--json-lines) printf '%s\n' '{"path":"safe/file","type":"file","size":4}' ;;
  *) exit 2 ;;
esac
"#
        .replace("__ARCHIVE_NAME__", &archive_name.display().to_string())
        .replace(
            "__REPOSITORY_LIST__",
            include_str!("fixtures/repository-list.json"),
        ),
    );
    let config = fake_config(temp.path(), script);
    fs::create_dir(temp.path().join("source")).unwrap();
    let backend = BorgBackend::new(&config);
    let index = Index::open(&config.index.path).unwrap();

    let before = boxup::domain::utc_now();
    let backup = JobRunner::new(&config, &backend, &index)
        .backup()
        .await
        .unwrap();
    assert_eq!(
        backup.notes,
        vec![boxup::domain::BackupNote::FilesChangedWhileBeingRead]
    );
    assert_eq!(backup.id, "c".repeat(64));
    let job = index.latest_job("backup").unwrap().unwrap();
    assert_eq!(job.state, boxup::domain::JobState::Succeeded);
    assert_eq!(job.archive_name.as_deref(), Some(backup.name.as_str()));
    assert_eq!(job.archive_id.as_deref(), Some(backup.id.as_str()));
    assert_eq!(
        job.message.as_deref(),
        Some("Completed with note: files changed while being read")
    );
    assert!(index.last_success("backup").unwrap().unwrap() >= before);

    let stamp: serde_json::Value = serde_json::from_slice(
        &fs::read(config.backup.state_dir.join("last-success.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stamp["archive"], backup.name);
    assert_eq!(stamp["archive_id"], backup.id);
    assert!(!index.status().unwrap().complete);
    assert!(index.snapshots().unwrap().is_empty());
    assert!(
        JobRunner::new(&config, &backend, &index)
            .backup_if_due()
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(index.recent_jobs(10).unwrap().len(), 1);
}

#[tokio::test]
async fn create_other_warning_codes_remain_errors() {
    for code in [1, 104] {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-borg");
        write_executable(
            &script,
            &format!(
                r#"#!/bin/sh
set -eu
cat >/dev/null
archive=
for argument in "$@"; do
  case "$argument" in ::*) archive=${{argument#::}} ;; esac
done
printf '%s\n' '{{"type":"log_message","levelname":"WARNING","message":"structured warning","msgid":"Warning"}}' >&2
printf '{{"archive":{{"id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","name":"%s","start":"2026-07-22T04:00:00Z"}}}}\n' "$archive"
exit {code}
"#
            ),
        );
        let config = fake_config(temp.path(), script);
        fs::create_dir(temp.path().join("source")).unwrap();
        let backend = BorgBackend::new(&config);
        let request = CreateRequest {
            archive_name: format!("test-warning-{code}"),
            sources: config.backup.sources.clone(),
            excludes: Vec::new(),
            one_file_system: true,
            exclude_caches: true,
            compression: "lz4".into(),
            upload_rate_kib: None,
        };

        let error = backend.create(&request).await.unwrap_err();
        assert!(format!("{error:#}").contains("structured warning"));
    }
}

#[tokio::test]
async fn create_code_100_requires_a_valid_expected_archive() {
    for output in [
        "{}",
        r#"{"archive":{"id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","name":"wrong-name","start":"2026-07-22T04:00:00Z"}}"#,
        r#"{"archive":{"id":"not-an-archive-id","name":"test-warning","start":"2026-07-22T04:00:00Z"}}"#,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-borg");
        write_executable(
            &script,
            &r#"#!/bin/sh
set -eu
cat >/dev/null
printf '%s\n' '__OUTPUT__'
exit 100
"#
            .replace("__OUTPUT__", output),
        );
        let config = fake_config(temp.path(), script);
        fs::create_dir(temp.path().join("source")).unwrap();
        let backend = BorgBackend::new(&config);
        let request = CreateRequest {
            archive_name: "test-warning".into(),
            sources: config.backup.sources.clone(),
            excludes: Vec::new(),
            one_file_system: true,
            exclude_caches: true,
            compression: "lz4".into(),
            upload_rate_kib: None,
        };

        assert!(backend.create(&request).await.is_err());
    }
}

#[tokio::test]
async fn create_cancellation_reaps_the_borg_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-borg");
    write_executable(
        &script,
        r#"#!/bin/sh
set -eu
cat >/dev/null
trap 'exit 130' INT TERM
case "${1:-}:${2:-}" in
  create:--json) while :; do sleep 1; done ;;
  *) exit 2 ;;
esac
"#,
    );
    let config = fake_config(temp.path(), script);
    fs::create_dir(temp.path().join("source")).unwrap();
    let backend = BorgBackend::new(&config);
    let request = CreateRequest {
        archive_name: "test-cancel".into(),
        sources: config.backup.sources.clone(),
        excludes: Vec::new(),
        one_file_system: true,
        exclude_caches: true,
        compression: "lz4".into(),
        upload_rate_kib: None,
    };
    let (progress, _progress_receiver) =
        tokio::sync::watch::channel(boxup::domain::CreateProgress::default());
    let (cancel, cancel_receiver) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = cancel.send(true);
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        backend.create_with_progress(&request, progress, cancel_receiver),
    )
    .await
    .expect("cancelled Borg did not exit in time");
    assert!(format!("{:#}", result.unwrap_err()).contains("cancelled"));
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
