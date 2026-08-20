use std::{sync::Arc, time::Duration};

use object_store::ObjectStore;

#[cfg(test)]
use std::pin::Pin;

#[cfg(test)]
use crate::{
    config::{ArchiveBackend, Config},
    error::AppError,
};
#[cfg(test)]
use bytes::Bytes;
#[cfg(test)]
use futures_util::{Stream, TryStreamExt, stream};
#[cfg(test)]
use object_store::{ObjectStoreExt, PutPayload, memory::InMemory, path::Path};

mod backend;
mod download;
mod multipart;
mod objects;
mod path;
mod readiness;
mod staging;

pub use download::ArchiveDownload;
#[cfg(test)]
use download::{validate_download_range, verified_download_stream};
pub use multipart::{ArchiveWriter, StagedArchiveObject};
#[cfg(test)]
use path::content_location;
pub use staging::ArchiveStagingObjectStore;

#[derive(Clone)]
pub struct ArchiveStore {
    inner: Arc<dyn ObjectStore>,
    readiness: Arc<tokio::sync::Mutex<ReadinessCache>>,
    readiness_path: object_store::path::Path,
}

struct ReadinessCache {
    last_success_at: Option<tokio::time::Instant>,
    failure_grace_until: Option<tokio::time::Instant>,
    next_check_at: Option<tokio::time::Instant>,
    refresh_jitter: Duration,
}

impl Default for ReadinessCache {
    fn default() -> Self {
        Self {
            last_success_at: None,
            failure_grace_until: None,
            next_check_at: None,
            refresh_jitter: readiness::refresh_jitter(uuid::Uuid::now_v7().as_u128() as u64),
        }
    }
}

#[cfg(test)]
mod tests;
