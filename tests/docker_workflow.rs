use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;
use boxup::backend::{Backend, DiffStream, FileStream};
use boxup::config::*;
use boxup::domain::{CreateRequest, RepositoryIdentity, Snapshot, utc_now};
use boxup::index::Index;
use boxup::jobs::{DockerManager, JobRunner};

#[tokio::test]
async fn audits_and_stages_a_two_pass_postgres_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let (config, action_log, rsync_log) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: true,
            confirmed_running: true,
            fail_start: false,
            include_unselected: false,
        },
    );
    let manager = DockerManager::new(&config);

    let audit = manager.audit(true).await.unwrap();
    assert_eq!(audit.containers.len(), 1);
    assert_eq!(audit.compose_projects, ["example-project"]);
    assert!(audit.containers[0].postgres);
    assert_eq!(
        audit.containers[0].mounts[0].name.as_deref(),
        Some("database")
    );

    let staging = manager.prepare_snapshot(&audit).await.unwrap().unwrap();
    assert_eq!(
        fs::read_to_string(staging.source.join("postgres/abc123.sql")).unwrap(),
        "-- logical dump --\n"
    );
    assert_eq!(
        fs::read_to_string(action_log).unwrap(),
        "stop abc123\nstart abc123\n"
    );
    let rsync_commands = fs::read_to_string(rsync_log).unwrap();
    assert_eq!(rsync_commands.lines().count(), 2);
    assert!(
        rsync_commands
            .lines()
            .all(|line| line.starts_with("-aHAXS --delete --numeric-ids -- "))
    );
    assert_eq!(staging.staged_sources, [temp.path().join("volume")]);
    assert!(!staging.source.join("quiesce-journal.json").exists());
}

#[tokio::test]
async fn resumes_after_failed_final_sync_and_recovers_a_saved_journal() {
    let temp = tempfile::tempdir().unwrap();
    let (config, action_log, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: true,
            running: true,
            confirmed_running: true,
            fail_start: false,
            include_unselected: false,
        },
    );
    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();

    assert!(manager.prepare_snapshot(&audit).await.is_err());
    assert_eq!(
        fs::read_to_string(&action_log).unwrap(),
        "stop abc123\nstart abc123\n"
    );
    let staging = config.docker.staging_dir.as_ref().unwrap();
    assert!(!staging.join("quiesce-journal.json").exists());

    fs::write(
        staging.join("quiesce-journal.json"),
        r#"{"active":["abc123"],"phase":"quiesced","created_at":"2026-07-22T00:00:00Z"}"#,
    )
    .unwrap();
    manager.recover_unfinished().await.unwrap();
    assert_eq!(
        fs::read_to_string(action_log).unwrap(),
        "stop abc123\nstart abc123\nstart abc123\n"
    );
    assert!(!staging.join("quiesce-journal.json").exists());
}

#[tokio::test]
async fn stopped_selected_container_is_staged_without_stop_start_or_dump() {
    let temp = tempfile::tempdir().unwrap();
    let (config, action_log, rsync_log) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: false,
            confirmed_running: false,
            fail_start: false,
            include_unselected: false,
        },
    );
    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();
    assert!(!audit.containers[0].running);

    let snapshot = manager.prepare_snapshot(&audit).await.unwrap().unwrap();
    assert_eq!(snapshot.staged_sources, [temp.path().join("volume")]);
    assert_eq!(fs::read_to_string(rsync_log).unwrap().lines().count(), 1);
    assert!(!action_log.exists());
    assert!(!snapshot.source.join("postgres/abc123.sql").exists());
}

#[tokio::test]
async fn restart_verification_failure_is_primary_and_retains_journal() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: true,
            confirmed_running: false,
            fail_start: false,
            include_unselected: false,
        },
    );
    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();
    let error = manager.prepare_snapshot(&audit).await.unwrap_err();
    assert!(format!("{error:#}").contains("restart/verification failed"));
    assert!(
        config
            .docker
            .staging_dir
            .as_ref()
            .unwrap()
            .join("quiesce-journal.json")
            .exists()
    );
}

#[tokio::test]
async fn restart_failure_overrides_staging_failure_and_recovery_keeps_journal_until_running() {
    let temp = tempfile::tempdir().unwrap();
    let (config, action_log, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: true,
            running: true,
            confirmed_running: true,
            fail_start: true,
            include_unselected: false,
        },
    );
    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();

    let error = manager.prepare_snapshot(&audit).await.unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("restart/verification failed after snapshot failure"));
    assert!(message.contains("start failed"));
    let journal = config
        .docker
        .staging_dir
        .as_ref()
        .unwrap()
        .join("quiesce-journal.json");
    assert!(journal.exists());

    fs::remove_file(temp.path().join("fail-start")).unwrap();
    manager.recover_unfinished().await.unwrap();
    assert!(!journal.exists());
    assert_eq!(
        fs::read_to_string(action_log).unwrap(),
        "stop abc123\nstart abc123\nstart abc123\n"
    );
}

#[tokio::test]
async fn backup_excludes_only_the_selected_mount_staged_this_run() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: false,
            confirmed_running: false,
            fail_start: false,
            include_unselected: true,
        },
    );
    let backend = RecordingBackend::default();
    let index = Index::open(&config.index.path).unwrap();

    JobRunner::new(&config, &backend, &index)
        .backup()
        .await
        .unwrap();
    let request = backend.request.lock().unwrap().clone().unwrap();
    let selected = literal_prefix(&temp.path().join("volume"));
    let unselected = literal_prefix(&temp.path().join("unselected-volume"));
    assert!(request.excludes.contains(&selected));
    assert!(!request.excludes.contains(&unselected));
}

#[tokio::test]
async fn mount_allowlist_stages_only_exact_sources_and_leaves_others_covered() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, _, rsync_log) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: false,
            confirmed_running: false,
            fail_start: false,
            include_unselected: true,
        },
    );
    let allowed = temp.path().join("unselected-volume");
    config.docker.stage_mounts = vec![allowed.clone()];
    config.docker.stop_containers.clear();
    config.docker.stop_all_stateful = true;
    config.validate().unwrap();
    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();
    assert_eq!(audit.containers.len(), 2);
    assert!(!audit.containers[0].stateful);
    assert!(audit.containers[1].stateful);
    assert_eq!(audit.containers[0].mounts.len(), 1);
    assert_eq!(
        audit.containers[0].mounts[0].source,
        temp.path().join("volume")
    );

    let backend = RecordingBackend::default();
    let index = Index::open(&config.index.path).unwrap();
    JobRunner::new(&config, &backend, &index)
        .backup()
        .await
        .unwrap();
    let request = backend.request.lock().unwrap().clone().unwrap();
    assert!(request.excludes.contains(&literal_prefix(&allowed)));
    assert!(
        !request
            .excludes
            .contains(&literal_prefix(&temp.path().join("volume")))
    );
    assert_eq!(fs::read_to_string(rsync_log).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn stages_a_bind_mounted_regular_file_without_directory_syntax() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _, rsync_log) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: false,
            confirmed_running: false,
            fail_start: false,
            include_unselected: false,
        },
    );
    let source = temp.path().join("volume");
    fs::remove_dir_all(&source).unwrap();
    fs::write(&source, "single file state").unwrap();

    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();
    let snapshot = manager.prepare_snapshot(&audit).await.unwrap().unwrap();
    let invocation = fs::read_to_string(rsync_log).unwrap();

    assert_eq!(snapshot.staged_sources, std::slice::from_ref(&source));
    assert!(invocation.contains(source.to_str().unwrap()));
    assert!(!invocation.contains(&format!("{}/", source.display())));
}

#[tokio::test]
async fn retries_transient_online_rsync_results() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: false,
            confirmed_running: false,
            fail_start: false,
            include_unselected: false,
        },
    );
    let count = temp.path().join("transient-count");
    write_executable(
        &config.docker.rsync_path,
        &format!(
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{0}' ] || count=$(cat '{0}')\ncount=$((count + 1))\nprintf '%s' \"$count\" >'{0}'\n[ \"$count\" -ge 3 ] || exit 23\n",
            count.display()
        ),
    );

    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();
    manager.prepare_snapshot(&audit).await.unwrap();

    assert_eq!(fs::read_to_string(count).unwrap(), "3");
}

#[tokio::test]
async fn final_rsync_result_23_is_not_retried() {
    let temp = tempfile::tempdir().unwrap();
    let (config, action_log, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: true,
            confirmed_running: true,
            fail_start: false,
            include_unselected: false,
        },
    );
    let count = temp.path().join("final-count");
    write_executable(
        &config.docker.rsync_path,
        &format!(
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{0}' ] || count=$(cat '{0}')\ncount=$((count + 1))\nprintf '%s' \"$count\" >'{0}'\n[ \"$count\" -ne 2 ] || exit 23\n",
            count.display()
        ),
    );

    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();
    assert!(manager.prepare_snapshot(&audit).await.is_err());

    assert_eq!(fs::read_to_string(count).unwrap(), "2");
    assert_eq!(
        fs::read_to_string(action_log).unwrap(),
        "stop abc123\nstart abc123\n"
    );
}

#[tokio::test]
async fn postgres_role_prefers_container_id_and_is_passed_as_direct_argv() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, _, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: true,
            confirmed_running: true,
            fail_start: false,
            include_unselected: false,
        },
    );
    config
        .docker
        .postgres_users
        .insert("database".into(), "ignored_name_role".into());
    config
        .docker
        .postgres_users
        .insert("abc123".into(), "backup_role".into());
    config.validate().unwrap();

    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();
    manager.prepare_snapshot(&audit).await.unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("docker-exec-argv")).unwrap(),
        "exec abc123 pg_dumpall -U backup_role\n"
    );
}

#[tokio::test]
async fn active_service_is_stopped_final_copied_restarted_and_verified() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, docker_log, rsync_log) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: true,
            confirmed_running: true,
            fail_start: false,
            include_unselected: false,
        },
    );
    let (service_path, service_log, _) =
        configure_fake_service(&mut config, temp.path(), true, false);
    let stale = config
        .docker
        .staging_dir
        .as_ref()
        .unwrap()
        .join("services/9");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("old"), "stale").unwrap();

    let backend = RecordingBackend::default();
    let index = Index::open(&config.index.path).unwrap();
    JobRunner::new(&config, &backend, &index)
        .backup()
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(service_log).unwrap(),
        "is-active --quiet app.service\nstop app.service\nstart app.service\nis-active --quiet app.service\n"
    );
    assert_eq!(
        fs::read_to_string(docker_log).unwrap(),
        "stop abc123\nstart abc123\n"
    );
    let rsync = fs::read_to_string(rsync_log).unwrap();
    assert_eq!(rsync.lines().count(), 4);
    assert_eq!(
        rsync
            .lines()
            .filter(|line| line.contains("/services/0"))
            .count(),
        2
    );
    assert!(
        backend
            .request
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .excludes
            .contains(&literal_prefix(&service_path))
    );
    assert!(!stale.exists());
}

#[tokio::test]
async fn inactive_service_is_never_stopped_or_started() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, docker_log, rsync_log) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: false,
            confirmed_running: false,
            fail_start: false,
            include_unselected: false,
        },
    );
    let (_, service_log, _) = configure_fake_service(&mut config, temp.path(), false, false);

    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();
    manager.prepare_snapshot(&audit).await.unwrap();
    assert_eq!(
        fs::read_to_string(service_log).unwrap(),
        "is-active --quiet app.service\n"
    );
    assert!(!docker_log.exists());
    assert_eq!(fs::read_to_string(rsync_log).unwrap().lines().count(), 2);
}

#[tokio::test]
async fn service_restart_failure_has_priority_and_journal_recovers_all_workloads() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, docker_log, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: false,
            running: true,
            confirmed_running: true,
            fail_start: false,
            include_unselected: false,
        },
    );
    let (_, service_log, fail_start) = configure_fake_service(&mut config, temp.path(), true, true);
    let manager = DockerManager::new(&config);
    let audit = manager.audit(true).await.unwrap();

    let error = manager.prepare_snapshot(&audit).await.unwrap_err();
    assert!(format!("{error:#}").contains("restart/verification failed"));
    let journal = config
        .docker
        .staging_dir
        .as_ref()
        .unwrap()
        .join("quiesce-journal.json");
    let journal_value: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    assert_eq!(
        journal_value["services"],
        serde_json::json!(["app.service"])
    );

    fs::remove_file(fail_start).unwrap();
    manager.recover_unfinished().await.unwrap();
    assert!(!journal.exists());
    assert_eq!(
        fs::read_to_string(docker_log).unwrap(),
        "stop abc123\nstart abc123\nstart abc123\n"
    );
    assert!(
        fs::read_to_string(service_log)
            .unwrap()
            .ends_with("start app.service\nis-active --quiet app.service\n")
    );
}

#[tokio::test]
async fn failed_staging_aborts_before_any_archive_can_exclude_the_mount() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _, _) = fake_docker_config(
        temp.path(),
        FakeOptions {
            fail_final_sync: true,
            running: true,
            confirmed_running: true,
            fail_start: false,
            include_unselected: false,
        },
    );
    let backend = RecordingBackend::default();
    let index = Index::open(&config.index.path).unwrap();

    assert!(
        JobRunner::new(&config, &backend, &index)
            .backup()
            .await
            .is_err()
    );
    assert!(backend.request.lock().unwrap().is_none());
    assert!(
        !config
            .docker
            .staging_dir
            .as_ref()
            .unwrap()
            .join("quiesce-journal.json")
            .exists()
    );
}

#[derive(Clone, Copy)]
struct FakeOptions {
    fail_final_sync: bool,
    running: bool,
    confirmed_running: bool,
    fail_start: bool,
    include_unselected: bool,
}

fn fake_docker_config(root: &Path, options: FakeOptions) -> (Config, PathBuf, PathBuf) {
    let docker = root.join("docker");
    let rsync = root.join("rsync");
    let action_log = root.join("docker-actions");
    let rsync_log = root.join("rsync-actions");
    let rsync_count = root.join("rsync-count");
    let volume = root.join("volume");
    fs::create_dir(&volume).unwrap();
    fs::write(volume.join("data"), "database files").unwrap();
    fs::create_dir(root.join("source")).unwrap();

    let mut inspect = vec![serde_json::json!({
        "Id": "abc123",
        "Name": "/database",
        "Config": {
            "Image": "postgres:17",
            "Labels": {"com.docker.compose.project": "example-project"}
        },
        "State": {"Running": options.running},
        "Mounts": [{
            "Type": "volume",
            "Source": &volume,
            "Destination": "/var/lib/postgresql/data",
            "Name": "database"
        }]
    })];
    if options.include_unselected {
        let unselected = root.join("unselected-volume");
        fs::create_dir(&unselected).unwrap();
        fs::write(unselected.join("data"), "other database files").unwrap();
        inspect.push(serde_json::json!({
            "Id": "def456",
            "Name": "/unselected",
            "Config": {"Image": "example/app:1", "Labels": {}},
            "State": {"Running": false},
            "Mounts": [{
                "Type": "bind",
                "Source": unselected,
                "Destination": "/srv/data",
                "Name": null
            }]
        }));
    }
    let ps_ids = if options.include_unselected {
        "abc123 def456"
    } else {
        "abc123"
    };
    let fail_start = root.join("fail-start");
    if options.fail_start {
        fs::write(&fail_start, "fail").unwrap();
    }
    let running_marker = root.join("running-marker");
    let exec_argv = root.join("docker-exec-argv");
    let confirm_start = if options.confirmed_running {
        format!(": >'{}'", running_marker.display())
    } else {
        format!("rm -f '{}'", running_marker.display())
    };
    let docker_script = format!(
        r#"#!/bin/sh
set -eu
case "${{1:-}}" in
  ps) printf '%s\n' {} ;;
  inspect)
    case "${{2:-}}" in
      --format=*)
        if [ -f '{}' ]; then printf '%s\n' true; else printf '%s\n' false; fi
        ;;
      *) printf '%s\n' '{}' ;;
    esac
    ;;
  exec)
    printf '%s\n' "$*" >>'{}'
    printf '%s\n' '-- logical dump --'
    ;;
  stop)
    printf '%s %s\n' "$1" "$2" >>'{}'
    rm -f '{}'
    ;;
  start)
    printf '%s %s\n' "$1" "$2" >>'{}'
    [ ! -f '{}' ] || exit 9
    {}
    ;;
  *) exit 2 ;;
esac
"#,
        ps_ids,
        running_marker.display(),
        serde_json::Value::Array(inspect),
        exec_argv.display(),
        action_log.display(),
        running_marker.display(),
        action_log.display(),
        fail_start.display(),
        confirm_start,
    );
    write_executable(&docker, &docker_script);

    let failure = if options.fail_final_sync {
        "[ \"$count\" -lt 2 ]"
    } else {
        "true"
    };
    let rsync_script = format!(
        r#"#!/bin/sh
set -eu
count=0
[ ! -f '{}' ] || count=$(cat '{}')
count=$((count + 1))
printf '%s\n' "$count" >'{}'
printf '%s\n' "$*" >>'{}'
{}
"#,
        rsync_count.display(),
        rsync_count.display(),
        rsync_count.display(),
        rsync_log.display(),
        failure
    );
    write_executable(&rsync, &rsync_script);

    let config = Config {
        source_path: None,
        version: 1,
        host: HostConfig { id: "test".into() },
        repository: RepositoryConfig {
            location: root.join("repo").display().to_string(),
            passphrase_file: root.join("passphrase"),
            ssh_key: root.join("key"),
            maintenance_ssh_key: None,
            known_hosts: root.join("known-hosts"),
            ssh_port: 22,
            borg_path: "/usr/bin/borg".into(),
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
            path: root.join("index/index.sqlite3"),
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
            enabled: true,
            staging_dir: Some(root.join("docker-staging")),
            stop_containers: if options.include_unselected {
                vec!["abc123".into()]
            } else {
                vec![]
            },
            stop_all_stateful: !options.include_unselected,
            stage_mounts: vec![],
            postgres_users: Default::default(),
            stop_services: vec![],
            service_paths: vec![],
            min_free_bytes: 1,
            docker_path: docker,
            rsync_path: rsync,
            systemctl_path: "/usr/bin/systemctl".into(),
        },
    };
    config.validate().unwrap();
    (config, action_log, rsync_log)
}

fn configure_fake_service(
    config: &mut Config,
    root: &Path,
    active: bool,
    fail_start: bool,
) -> (PathBuf, PathBuf, PathBuf) {
    let systemctl = root.join("systemctl");
    let service_log = root.join("service-actions");
    let active_marker = root.join("service-active");
    let fail_start_marker = root.join("service-fail-start");
    let service_path = root.join("service-data");
    fs::create_dir(&service_path).unwrap();
    fs::write(service_path.join("data"), "service files").unwrap();
    if active {
        fs::write(&active_marker, "active").unwrap();
    }
    if fail_start {
        fs::write(&fail_start_marker, "fail").unwrap();
    }
    let script = format!(
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >>'{}'
case "${{1:-}}" in
  is-active)
    [ -f '{}' ] || exit 3
    ;;
  stop)
    rm -f '{}'
    ;;
  start)
    [ ! -f '{}' ] || exit 9
    : >'{}'
    ;;
  *) exit 2 ;;
esac
"#,
        service_log.display(),
        active_marker.display(),
        active_marker.display(),
        fail_start_marker.display(),
        active_marker.display(),
    );
    write_executable(&systemctl, &script);
    config.docker.stop_services = vec!["app.service".into()];
    config.docker.service_paths = vec![service_path.clone()];
    config.docker.systemctl_path = systemctl;
    config.validate().unwrap();
    (service_path, service_log, fail_start_marker)
}

fn literal_prefix(path: &Path) -> String {
    format!("pp:{}", path.strip_prefix("/").unwrap().to_str().unwrap())
}

#[derive(Default)]
struct RecordingBackend {
    request: Mutex<Option<CreateRequest>>,
}

#[async_trait]
impl Backend for RecordingBackend {
    async fn preflight(&self) -> Result<()> {
        Ok(())
    }

    async fn repository_exists(&self) -> Result<bool> {
        Ok(true)
    }

    async fn init_repository(&self) -> Result<()> {
        bail!("unexpected init")
    }

    async fn repository_identity(&self) -> Result<RepositoryIdentity> {
        Ok(RepositoryIdentity {
            id: "a".repeat(64),
            location: "fake-repository".into(),
        })
    }

    async fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
        Ok(Vec::new())
    }

    async fn list_files(&self, _snapshot: &str, _path: Option<&str>) -> Result<FileStream> {
        bail!("unexpected file listing")
    }

    async fn create(&self, request: &CreateRequest) -> Result<Snapshot> {
        *self.request.lock().unwrap() = Some(request.clone());
        Ok(Snapshot {
            id: "c".repeat(64),
            name: request.archive_name.clone(),
            start: utc_now(),
            end: Some(utc_now()),
            hostname: Some("test".into()),
            username: Some("tester".into()),
        })
    }

    async fn extract(&self, _snapshot: &str, _paths: &[String], _destination: &Path) -> Result<()> {
        bail!("unexpected extract")
    }

    async fn mount(&self, _snapshot: &str, _target: &Path) -> Result<()> {
        bail!("unexpected mount")
    }

    async fn umount(&self, _target: &Path) -> Result<()> {
        bail!("unexpected unmount")
    }

    async fn diff(&self, _a: &str, _b: &str, _path: Option<&str>) -> Result<DiffStream> {
        bail!("unexpected diff")
    }

    async fn prune(
        &self,
        _archive_prefix: &str,
        _keep: (u32, u32, u32),
        _dry_run: bool,
    ) -> Result<()> {
        bail!("unexpected prune")
    }

    async fn compact(&self) -> Result<()> {
        bail!("unexpected compact")
    }

    async fn check(&self, _verify_data: bool) -> Result<()> {
        bail!("unexpected check")
    }

    async fn key_export(&self, _destination: &Path) -> Result<()> {
        bail!("unexpected key export")
    }
}

fn write_executable(path: &Path, content: &str) {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = fs::File::create(&temporary).unwrap();
    file.write_all(content.as_bytes()).unwrap();
    file.sync_all().unwrap();
    let mut permissions = file.metadata().unwrap().permissions();
    permissions.set_mode(0o700);
    file.set_permissions(permissions).unwrap();
    drop(file);
    fs::rename(temporary, path).unwrap();
}
