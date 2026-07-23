use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub fn utc_now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
    pub hostname: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryIdentity {
    pub id: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveItem {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: FileType,
    #[serde(default)]
    pub size: u64,
    pub mtime: Option<DateTime<Utc>>,
    pub mode: Option<String>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub user: Option<String>,
    pub group: Option<String>,
    pub link_target: Option<String>,
    pub health: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileType {
    File,
    Directory,
    Symlink,
    Fifo,
    BlockDevice,
    CharDevice,
    Socket,
    Other,
}

impl FileType {
    pub fn from_borg(value: &str) -> Self {
        match value {
            "-" | "file" => Self::File,
            "d" | "directory" => Self::Directory,
            "l" | "symlink" => Self::Symlink,
            "p" | "fifo" => Self::Fifo,
            "b" | "block" | "block_device" => Self::BlockDevice,
            "c" | "char" | "char_device" => Self::CharDevice,
            "s" | "socket" => Self::Socket,
            _ => Self::Other,
        }
    }

    pub fn is_special(self) -> bool {
        matches!(
            self,
            Self::Fifo | Self::BlockDevice | Self::CharDevice | Self::Socket | Self::Other
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: String,
    pub change: String,
}

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub archive_name: String,
    pub sources: Vec<PathBuf>,
    pub excludes: Vec<String>,
    pub one_file_system: bool,
    pub exclude_caches: bool,
    pub compression: String,
    pub upload_rate_kib: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: i64,
    pub kind: String,
    pub state: JobState,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub message: Option<String>,
}
