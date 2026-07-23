use std::path::Path;
use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;

use crate::domain::{
    ArchiveItem, CreateRequest, DiffEntry, ExtractProgress, RepositoryIdentity, Snapshot,
};

pub type FileStream = Pin<Box<dyn Stream<Item = Result<ArchiveItem>> + Send>>;
pub type DiffStream = Pin<Box<dyn Stream<Item = Result<DiffEntry>> + Send>>;

#[async_trait]
pub trait Backend: Send + Sync {
    async fn preflight(&self) -> Result<()>;
    async fn repository_exists(&self) -> Result<bool>;
    async fn init_repository(&self) -> Result<()>;
    async fn repository_identity(&self) -> Result<RepositoryIdentity>;
    async fn list_snapshots(&self) -> Result<Vec<Snapshot>>;
    async fn list_files(&self, snapshot: &str, path: Option<&str>) -> Result<FileStream>;
    async fn list_files_selected(&self, snapshot: &str, paths: &[String]) -> Result<FileStream> {
        let stream = self.list_files(snapshot, None).await?;
        let paths = paths.to_vec();
        Ok(Box::pin(futures::TryStreamExt::try_filter(
            stream,
            move |item| {
                let included = paths.iter().any(|path| {
                    item.path == *path
                        || item
                            .path
                            .strip_prefix(path)
                            .is_some_and(|tail| tail.starts_with('/'))
                        || path
                            .strip_prefix(&item.path)
                            .is_some_and(|tail| tail.starts_with('/'))
                });
                futures::future::ready(included)
            },
        )))
    }
    async fn create(&self, request: &CreateRequest) -> Result<Snapshot>;
    async fn extract(&self, snapshot: &str, paths: &[String], destination: &Path) -> Result<()>;
    async fn extract_with_progress(
        &self,
        snapshot: &str,
        paths: &[String],
        destination: &Path,
        progress: tokio::sync::watch::Sender<ExtractProgress>,
    ) -> Result<()> {
        let result = self.extract(snapshot, paths, destination).await;
        drop(progress);
        result
    }
    async fn mount(&self, snapshot: &str, target: &Path) -> Result<()>;
    async fn umount(&self, target: &Path) -> Result<()>;
    async fn diff(&self, a: &str, b: &str, path: Option<&str>) -> Result<DiffStream>;
    async fn prune(&self, archive_prefix: &str, keep: (u32, u32, u32), dry_run: bool)
    -> Result<()>;
    async fn compact(&self) -> Result<()>;
    async fn check(&self, verify_data: bool) -> Result<()>;
    async fn key_export(&self, destination: &Path) -> Result<()>;
}
