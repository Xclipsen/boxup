use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use crate::backend::Backend;
use crate::domain::{
    ArchiveItem, FileType, JobRecord, JobState, RepositoryIdentity, Snapshot, utc_now,
};

pub const INDEX_SCHEMA_VERSION: u32 = 1;

pub struct Index {
    path: PathBuf,
    read_only: bool,
}

impl Index {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let existed = path.exists();
        match fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "index must be a regular non-symlink file"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create index directory {}", parent.display())
            })?;
        }
        let index = Self {
            path,
            read_only: false,
        };
        let connection = index.connect()?;
        initialize(&connection)?;
        if !existed {
            let mut permissions = fs::metadata(&index.path)?.permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&index.path, permissions)?;
        }
        Ok(index)
    }

    pub fn open_read_only(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("index is unavailable: {}", path.display()))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "index must be a regular non-symlink file"
        );
        let index = Self {
            path,
            read_only: true,
        };
        index.connect()?;
        Ok(index)
    }

    pub fn exists(path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() > 0
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn refresh<B: Backend + ?Sized>(&self, backend: &B) -> Result<RefreshStats> {
        let identity = backend.repository_identity().await?;
        validate_repository_identity(&identity)?;
        let generation = self.mark_refresh_started(&identity)?;
        let live = backend.list_snapshots().await?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let existing = archive_ids(&transaction)?;
        let live_ids: HashMap<_, _> = live
            .iter()
            .map(|snapshot| (snapshot.name.as_str(), snapshot.id.as_str()))
            .collect();
        let stale: Vec<_> = existing
            .iter()
            .filter(|(name, id)| live_ids.get(name.as_str()).copied() != Some(id.as_str()))
            .map(|(name, _)| name.clone())
            .collect();
        let removed = stale.len() as u64;
        for name in &stale {
            transaction.execute("DELETE FROM archives WHERE name = ?1", [name])?;
        }

        let mut archives_added = 0;
        let mut files_added = 0;
        for snapshot in live
            .iter()
            .filter(|snapshot| existing.get(&snapshot.name) != Some(&snapshot.id))
        {
            let archive_id = insert_archive(&transaction, snapshot)?;
            let mut stream = backend.list_files(&snapshot.name, None).await?;
            while let Some(item) = stream.try_next().await? {
                insert_item(&transaction, archive_id, &item)?;
                files_added += 1;
            }
            archives_added += 1;
        }
        let completed_identity = backend.repository_identity().await?;
        validate_repository_identity(&completed_identity)?;
        ensure!(
            completed_identity == identity,
            "repository identity changed during index refresh"
        );
        let generation = generation
            .checked_add(1)
            .context("index refresh generation overflow")?;
        transaction.execute(
            "INSERT INTO index_meta(
                singleton, schema_version, repository_id, repository_location,
                refresh_generation, refreshed_at, complete
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, 1)
             ON CONFLICT(singleton) DO UPDATE SET
               schema_version = excluded.schema_version,
               repository_id = excluded.repository_id,
               repository_location = excluded.repository_location,
               refresh_generation = excluded.refresh_generation,
               refreshed_at = excluded.refreshed_at,
               complete = excluded.complete",
            params![
                INDEX_SCHEMA_VERSION,
                identity.id,
                identity.location,
                generation,
                utc_now().to_rfc3339()
            ],
        )?;
        transaction.commit()?;
        Ok(RefreshStats {
            archives_added,
            archives_removed: removed,
            files_added,
        })
    }

    pub fn snapshots(&self) -> Result<Vec<Snapshot>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT borg_id, name, start, end, hostname, username
             FROM archives ORDER BY start DESC",
        )?;
        let rows = statement.query_map([], snapshot_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn status(&self) -> Result<IndexStatus> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT schema_version, repository_id, repository_location,
                        refresh_generation, refreshed_at, complete
                  FROM index_meta WHERE singleton = 1",
                [],
                |row| {
                    let refreshed_at = row
                        .get::<_, Option<String>>(4)?
                        .map(|value| parse_time(value, 4))
                        .transpose()?;
                    Ok(IndexStatus {
                        schema_version: nonnegative_u32(row.get(0)?, 0)?,
                        repository_id: row.get(1)?,
                        repository_location: row.get(2)?,
                        refresh_generation: nonnegative_u64(row.get(3)?, 3)?,
                        refreshed_at,
                        complete: row.get::<_, i64>(5)? == 1,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    pub fn is_usable(&self, repository_location: &str, max_age: Duration) -> Result<bool> {
        let status = self.status()?;
        Ok(status.is_complete()
            && status.repository_location.as_deref() == Some(repository_location)
            && status.is_fresh(max_age)?)
    }

    pub fn is_complete(&self) -> Result<bool> {
        Ok(self.status()?.is_complete())
    }

    pub fn is_fresh(&self, max_age: Duration) -> Result<bool> {
        self.status()?.is_fresh(max_age)
    }

    pub fn is_complete_for(&self, repository: &RepositoryIdentity) -> Result<bool> {
        let status = self.status()?;
        Ok(status.is_complete() && status.matches_repository(repository))
    }

    pub async fn is_usable_for_live_repository<B: Backend + ?Sized>(
        &self,
        backend: &B,
        max_age: Duration,
    ) -> Result<bool> {
        let repository = backend.repository_identity().await?;
        validate_repository_identity(&repository)?;
        let status = self.status()?;
        Ok(status.is_complete()
            && status.matches_repository(&repository)
            && status.is_fresh(max_age)?)
    }

    pub fn list_files(&self, snapshot: &str, prefix: Option<&str>) -> Result<Vec<ArchiveItem>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT f.path, f.type, f.size, f.mtime, f.mode, f.uid, f.gid,
                    f.user_name, f.group_name, f.link_target, f.health
             FROM files f JOIN archives a ON a.id = f.archive_id
             WHERE a.name = ?1 AND (?2 IS NULL OR f.path = ?2 OR f.path LIKE ?3 ESCAPE '\\')
             ORDER BY f.path",
        )?;
        let escaped = prefix.map(|value| format!("{}/%", escape_like(value.trim_end_matches('/'))));
        let rows = statement.query_map(params![snapshot, prefix, escaped], item_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn directory_entries(&self, snapshot: &str, parent: &str) -> Result<Vec<ArchiveItem>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT f.path, f.type, f.size, f.mtime, f.mode, f.uid, f.gid,
                    f.user_name, f.group_name, f.link_target, f.health
             FROM files f JOIN archives a ON a.id = f.archive_id
             WHERE a.name = ?1 AND f.parent = ?2 ORDER BY f.type = 'directory' DESC, f.name",
        )?;
        let rows = statement.query_map(params![snapshot, parent], item_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn search(&self, query: &str, all_snapshots: bool) -> Result<Vec<SearchResult>> {
        let query = query.trim();
        ensure!(!query.is_empty(), "search query cannot be empty");
        let connection = self.connect()?;
        let latest: Option<String> = if all_snapshots {
            None
        } else {
            connection
                .query_row(
                    "SELECT name FROM archives ORDER BY start DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?
        };
        self.search_in_snapshot(query, latest.as_deref())
    }

    pub fn search_snapshot(&self, query: &str, snapshot: &str) -> Result<Vec<SearchResult>> {
        let query = query.trim();
        ensure!(!query.is_empty(), "search query cannot be empty");
        self.search_in_snapshot(query, Some(snapshot))
    }

    fn search_in_snapshot(&self, query: &str, snapshot: Option<&str>) -> Result<Vec<SearchResult>> {
        let connection = self.connect()?;
        if query.chars().count() >= 3 {
            let fts_query = format!("\"{}\"", query.replace('"', "\"\""));
            let mut statement = connection.prepare(
                "SELECT a.name, f.path, f.type, f.size, f.mtime
                 FROM files_fts
                 JOIN files f ON f.id = files_fts.rowid
                 JOIN archives a ON a.id = f.archive_id
                 WHERE files_fts MATCH ?1 AND (?2 IS NULL OR a.name = ?2)
                 ORDER BY a.start DESC, f.path LIMIT 1000",
            )?;
            let rows = statement.query_map(params![fts_query, snapshot], search_from_row)?;
            return rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into);
        }
        let like = format!("%{}%", escape_like(query));
        let mut statement = connection.prepare(
            "SELECT a.name, f.path, f.type, f.size, f.mtime
             FROM files f JOIN archives a ON a.id = f.archive_id
             WHERE f.path LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR a.name = ?2)
             ORDER BY a.start DESC, f.path LIMIT 1000",
        )?;
        let rows = statement.query_map(params![like, snapshot], search_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn start_job(&self, kind: &str) -> Result<i64> {
        let connection = self.connect()?;
        connection.execute(
            "INSERT INTO jobs(kind, state, started_at) VALUES (?1, 'running', ?2)",
            params![kind, utc_now().to_rfc3339()],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn finish_job(&self, id: i64, success: bool, message: Option<&str>) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE jobs SET state = ?1, finished_at = ?2, message = ?3 WHERE id = ?4",
            params![
                if success { "succeeded" } else { "failed" },
                utc_now().to_rfc3339(),
                message,
                id
            ],
        )?;
        Ok(())
    }

    pub fn recent_jobs(&self, limit: u32) -> Result<Vec<JobRecord>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT id, kind, state, started_at, finished_at, message
             FROM jobs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            let state: String = row.get(2)?;
            Ok(JobRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                state: match state.as_str() {
                    "succeeded" => JobState::Succeeded,
                    "failed" => JobState::Failed,
                    _ => JobState::Running,
                },
                started_at: parse_time(row.get::<_, String>(3)?, 3)?,
                finished_at: row
                    .get::<_, Option<String>>(4)?
                    .map(|value| parse_time(value, 4))
                    .transpose()?,
                message: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn last_success(&self, kind: &str) -> Result<Option<DateTime<Utc>>> {
        let connection = self.connect()?;
        let value: Option<String> = connection
            .query_row(
                "SELECT finished_at FROM jobs WHERE kind = ?1 AND state = 'succeeded'
                 ORDER BY finished_at DESC LIMIT 1",
                [kind],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        value.map(|value| parse_datetime(&value)).transpose()
    }

    fn connect(&self) -> Result<Connection> {
        let connection = if self.read_only {
            Connection::open_with_flags(
                &self.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
        } else {
            Connection::open(&self.path)
        }
        .with_context(|| format!("failed to open index {}", self.path.display()))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        if !self.read_only {
            connection.pragma_update(None, "journal_mode", "DELETE")?;
        }
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        Ok(connection)
    }

    fn mark_refresh_started(&self, identity: &RepositoryIdentity) -> Result<u64> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let previous = transaction
            .query_row(
                "SELECT repository_id, refresh_generation, refreshed_at
                 FROM index_meta WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        nonnegative_u64(row.get(1)?, 1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let same_repository = previous
            .as_ref()
            .and_then(|(repository_id, _, _)| repository_id.as_deref())
            == Some(identity.id.as_str());
        let generation = previous
            .as_ref()
            .filter(|_| same_repository)
            .map_or(0, |(_, generation, _)| *generation);
        let refreshed_at = previous
            .as_ref()
            .filter(|_| same_repository)
            .and_then(|(_, _, refreshed_at)| refreshed_at.as_deref());
        if !same_repository {
            transaction.execute("DELETE FROM archives", [])?;
        }
        transaction.execute(
            "INSERT INTO index_meta(
                singleton, schema_version, repository_id, repository_location,
                refresh_generation, refreshed_at, complete
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, 0)
             ON CONFLICT(singleton) DO UPDATE SET
               schema_version = excluded.schema_version,
               repository_id = excluded.repository_id,
               repository_location = excluded.repository_location,
               refresh_generation = excluded.refresh_generation,
               refreshed_at = excluded.refreshed_at,
               complete = excluded.complete",
            params![
                INDEX_SCHEMA_VERSION,
                identity.id,
                identity.location,
                generation,
                refreshed_at
            ],
        )?;
        transaction.commit()?;
        Ok(generation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshStats {
    pub archives_added: u64,
    pub archives_removed: u64,
    pub files_added: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct IndexStatus {
    pub schema_version: u32,
    pub repository_id: Option<String>,
    pub repository_location: Option<String>,
    pub refresh_generation: u64,
    pub refreshed_at: Option<DateTime<Utc>>,
    pub complete: bool,
}

impl IndexStatus {
    pub fn is_complete(&self) -> bool {
        self.complete
            && self.schema_version == INDEX_SCHEMA_VERSION
            && self.refresh_generation > 0
            && self.refreshed_at.is_some()
            && self
                .repository_id
                .as_deref()
                .is_some_and(valid_repository_id)
            && self
                .repository_location
                .as_deref()
                .is_some_and(|value| !value.is_empty() && !value.chars().any(char::is_control))
    }

    pub fn is_fresh(&self, max_age: Duration) -> Result<bool> {
        if !self.is_complete() {
            return Ok(false);
        }
        let refreshed_at = self.refreshed_at.expect("complete status has a timestamp");
        let age = utc_now().signed_duration_since(refreshed_at);
        Ok(age >= chrono::Duration::zero() && age <= chrono::Duration::from_std(max_age)?)
    }

    pub fn matches_repository(&self, repository: &RepositoryIdentity) -> bool {
        self.repository_id.as_deref() == Some(repository.id.as_str())
            && self.repository_location.as_deref() == Some(repository.location.as_str())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub snapshot: String,
    pub path: String,
    pub kind: FileType,
    pub size: u64,
    pub mtime: Option<DateTime<Utc>>,
}

fn initialize(connection: &Connection) -> Result<()> {
    let existing_version: u32 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(
        existing_version <= INDEX_SCHEMA_VERSION,
        "index schema version {existing_version} is newer than supported version {INDEX_SCHEMA_VERSION}"
    );
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS archives (
            id INTEGER PRIMARY KEY,
            borg_id TEXT NOT NULL,
            name TEXT NOT NULL UNIQUE,
            start TEXT NOT NULL,
            end TEXT,
            hostname TEXT,
            username TEXT
        );
        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            archive_id INTEGER NOT NULL REFERENCES archives(id) ON DELETE CASCADE,
            path TEXT NOT NULL,
            parent TEXT NOT NULL,
            name TEXT NOT NULL,
            type TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime TEXT,
            mode TEXT,
            uid INTEGER,
            gid INTEGER,
            user_name TEXT,
            group_name TEXT,
            link_target TEXT,
            health TEXT,
            UNIQUE(archive_id, path)
        );
        CREATE INDEX IF NOT EXISTS files_archive_parent ON files(archive_id, parent, name);
        CREATE INDEX IF NOT EXISTS files_archive_path ON files(archive_id, path);
        CREATE INDEX IF NOT EXISTS archives_start ON archives(start DESC);
        CREATE TABLE IF NOT EXISTS index_meta (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            schema_version INTEGER NOT NULL,
            repository_id TEXT,
            repository_location TEXT,
            refresh_generation INTEGER NOT NULL,
            refreshed_at TEXT,
            complete INTEGER NOT NULL CHECK(complete IN (0, 1))
        );
        CREATE TABLE IF NOT EXISTS jobs (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('running', 'succeeded', 'failed')),
            started_at TEXT NOT NULL,
            finished_at TEXT,
            message TEXT
        );
        CREATE INDEX IF NOT EXISTS jobs_kind_finished ON jobs(kind, finished_at DESC);
        CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
            path, content='files', content_rowid='id', tokenize='trigram'
        );
        CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
            INSERT INTO files_fts(rowid, path) VALUES (new.id, new.path);
        END;
        CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
            INSERT INTO files_fts(files_fts, rowid, path) VALUES ('delete', old.id, old.path);
        END;
        CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
            INSERT INTO files_fts(files_fts, rowid, path) VALUES ('delete', old.id, old.path);
            INSERT INTO files_fts(rowid, path) VALUES (new.id, new.path);
        END;",
    )?;
    if !table_has_column(connection, "index_meta", "schema_version")? {
        connection.execute(
            "ALTER TABLE index_meta ADD COLUMN schema_version INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    if !table_has_column(connection, "index_meta", "refresh_generation")? {
        connection.execute(
            "ALTER TABLE index_meta ADD COLUMN refresh_generation INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    connection.execute(
        "INSERT OR IGNORE INTO index_meta(
            singleton, schema_version, repository_id, repository_location,
            refresh_generation, refreshed_at, complete
         ) VALUES (1, ?1, NULL, NULL, 0, NULL, 0)",
        [INDEX_SCHEMA_VERSION],
    )?;
    connection.pragma_update(None, "user_version", INDEX_SCHEMA_VERSION)?;
    Ok(())
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for candidate in columns {
        if candidate? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn archive_ids(transaction: &Transaction<'_>) -> Result<HashMap<String, String>> {
    let mut statement = transaction.prepare("SELECT name, borg_id FROM archives")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn insert_archive(transaction: &Transaction<'_>, snapshot: &Snapshot) -> Result<i64> {
    transaction.execute(
        "INSERT INTO archives(borg_id, name, start, end, hostname, username)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            snapshot.id,
            snapshot.name,
            snapshot.start.to_rfc3339(),
            snapshot.end.map(|value| value.to_rfc3339()),
            snapshot.hostname,
            snapshot.username
        ],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn insert_item(transaction: &Transaction<'_>, archive_id: i64, item: &ArchiveItem) -> Result<()> {
    let (parent, name) = split_parent(&item.path);
    transaction.execute(
        "INSERT INTO files(
            archive_id, path, parent, name, type, size, mtime, mode, uid, gid,
            user_name, group_name, link_target, health
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            archive_id,
            item.path,
            parent,
            name,
            file_type_name(item.kind),
            i64::try_from(item.size).unwrap_or(i64::MAX),
            item.mtime.map(|value| value.to_rfc3339()),
            item.mode,
            item.uid,
            item.gid,
            item.user,
            item.group,
            item.link_target,
            item.health
        ],
    )?;
    Ok(())
}

fn split_parent(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn file_type_name(kind: FileType) -> &'static str {
    match kind {
        FileType::File => "file",
        FileType::Directory => "directory",
        FileType::Symlink => "symlink",
        FileType::Fifo => "fifo",
        FileType::BlockDevice => "block_device",
        FileType::CharDevice => "char_device",
        FileType::Socket => "socket",
        FileType::Other => "other",
    }
}

fn parse_file_type(value: &str) -> FileType {
    FileType::from_borg(value)
}

fn snapshot_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Snapshot> {
    Ok(Snapshot {
        id: row.get(0)?,
        name: row.get(1)?,
        start: parse_time(row.get::<_, String>(2)?, 2)?,
        end: row
            .get::<_, Option<String>>(3)?
            .map(|value| parse_time(value, 3))
            .transpose()?,
        hostname: row.get(4)?,
        username: row.get(5)?,
    })
}

fn item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArchiveItem> {
    let kind: String = row.get(1)?;
    let size: i64 = row.get(2)?;
    Ok(ArchiveItem {
        path: row.get(0)?,
        kind: parse_file_type(&kind),
        size: size.max(0) as u64,
        mtime: row
            .get::<_, Option<String>>(3)?
            .map(|value| parse_time(value, 3))
            .transpose()?,
        mode: row.get(4)?,
        uid: row.get(5)?,
        gid: row.get(6)?,
        user: row.get(7)?,
        group: row.get(8)?,
        link_target: row.get(9)?,
        health: row.get(10)?,
    })
}

fn search_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchResult> {
    let kind: String = row.get(2)?;
    let size: i64 = row.get(3)?;
    Ok(SearchResult {
        snapshot: row.get(0)?,
        path: row.get(1)?,
        kind: parse_file_type(&kind),
        size: size.max(0) as u64,
        mtime: row
            .get::<_, Option<String>>(4)?
            .map(|value| parse_time(value, 4))
            .transpose()?,
    })
}

fn valid_repository_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_repository_identity(identity: &RepositoryIdentity) -> Result<()> {
    ensure!(
        valid_repository_id(&identity.id),
        "invalid repository identity"
    );
    ensure!(
        !identity.location.is_empty() && !identity.location.chars().any(char::is_control),
        "invalid repository location"
    );
    Ok(())
}

fn nonnegative_u32(value: i64, column: usize) -> rusqlite::Result<u32> {
    u32::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn nonnegative_u64(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn parse_time(value: String, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    parse_datetime(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, error.into())
    })
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_split_is_archive_relative() {
        assert_eq!(split_parent("etc/hosts"), ("etc", "hosts"));
        assert_eq!(split_parent("etc"), ("", "etc"));
    }

    #[test]
    fn creates_fts_schema() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        assert!(index.snapshots().unwrap().is_empty());
        assert_eq!(
            index.status().unwrap(),
            IndexStatus {
                schema_version: INDEX_SCHEMA_VERSION,
                ..IndexStatus::default()
            }
        );
        assert!(!index.is_complete().unwrap());
        drop(index);
        assert!(
            Index::open_read_only(temp.path().join("index.sqlite3"))
                .unwrap()
                .snapshots()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn usability_requires_matching_complete_recent_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let connection = index.connect().unwrap();
        connection
            .execute(
                "UPDATE index_meta SET
                   repository_id = ?1, repository_location = ?2,
                   refresh_generation = 1, refreshed_at = ?3, complete = 1",
                params!["a".repeat(64), "/repo", utc_now().to_rfc3339()],
            )
            .unwrap();
        assert!(index.is_usable("/repo", Duration::from_secs(60)).unwrap());
        assert!(!index.is_usable("/other", Duration::from_secs(60)).unwrap());

        connection
            .execute(
                "UPDATE index_meta SET refreshed_at = ?1",
                [(utc_now() - chrono::Duration::minutes(2)).to_rfc3339()],
            )
            .unwrap();
        assert!(!index.is_usable("/repo", Duration::from_secs(60)).unwrap());
        connection
            .execute("UPDATE index_meta SET complete = 0", [])
            .unwrap();
        assert!(!index.is_usable("/repo", Duration::from_secs(600)).unwrap());
    }

    #[test]
    fn empty_or_symlink_database_is_not_reported_as_existing() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let empty = temp.path().join("empty.sqlite3");
        fs::write(&empty, []).unwrap();
        assert!(!Index::exists(&empty));

        let index_path = temp.path().join("index.sqlite3");
        Index::open(&index_path).unwrap();
        assert!(Index::exists(&index_path));
        let link = temp.path().join("index-link.sqlite3");
        symlink(&index_path, &link).unwrap();
        assert!(!Index::exists(&link));
    }

    #[test]
    fn completeness_can_be_bound_to_an_exact_repository_identity() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let connection = index.connect().unwrap();
        connection
            .execute(
                "UPDATE index_meta SET
                   repository_id = ?1, repository_location = ?2,
                   refresh_generation = 7, refreshed_at = ?3, complete = 1",
                params!["a".repeat(64), "/repo", utc_now().to_rfc3339()],
            )
            .unwrap();
        let expected = RepositoryIdentity {
            id: "a".repeat(64),
            location: "/repo".into(),
        };
        let replacement = RepositoryIdentity {
            id: "b".repeat(64),
            location: "/repo".into(),
        };

        assert!(index.is_complete_for(&expected).unwrap());
        assert!(!index.is_complete_for(&replacement).unwrap());
        assert!(index.is_fresh(Duration::from_secs(60)).unwrap());
    }

    #[test]
    fn writable_open_migrates_pre_metadata_schema() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE index_meta (
                    singleton INTEGER PRIMARY KEY,
                    repository_id TEXT,
                    repository_location TEXT,
                    refreshed_at TEXT,
                    complete INTEGER NOT NULL
                 );
                 INSERT INTO index_meta VALUES (1, NULL, NULL, NULL, 0);",
            )
            .unwrap();
        drop(connection);

        let index = Index::open(&path).unwrap();
        let status = index.status().unwrap();
        assert_eq!(status.schema_version, INDEX_SCHEMA_VERSION);
        assert_eq!(status.refresh_generation, 0);
        assert!(!status.complete);
    }

    #[test]
    fn snapshot_search_filters_before_applying_result_limit() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let mut connection = index.connect().unwrap();
        let transaction = connection.transaction().unwrap();
        let active = Snapshot {
            id: "a".repeat(64),
            name: "active".into(),
            start: DateTime::UNIX_EPOCH,
            end: None,
            hostname: None,
            username: None,
        };
        let other = Snapshot {
            id: "b".repeat(64),
            name: "newer".into(),
            start: DateTime::UNIX_EPOCH + chrono::Duration::days(1),
            end: None,
            hostname: None,
            username: None,
        };
        let active_id = insert_archive(&transaction, &active).unwrap();
        let other_id = insert_archive(&transaction, &other).unwrap();
        insert_item(
            &transaction,
            active_id,
            &ArchiveItem {
                path: "wanted/needle".into(),
                kind: FileType::File,
                size: 1,
                mtime: None,
                mode: None,
                uid: None,
                gid: None,
                user: None,
                group: None,
                link_target: None,
                health: None,
            },
        )
        .unwrap();
        for position in 0..1000 {
            insert_item(
                &transaction,
                other_id,
                &ArchiveItem {
                    path: format!("other/needle-{position:04}"),
                    kind: FileType::File,
                    size: 1,
                    mtime: None,
                    mode: None,
                    uid: None,
                    gid: None,
                    user: None,
                    group: None,
                    link_target: None,
                    health: None,
                },
            )
            .unwrap();
        }
        transaction.commit().unwrap();

        let results = index.search_snapshot("needle", "active").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snapshot, "active");
        assert_eq!(results[0].path, "wanted/needle");
    }
}
