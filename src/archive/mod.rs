use std::sync::Arc;

use object_store::ObjectStore;

#[cfg(test)]
use std::{pin::Pin, time::Duration};

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
}

#[derive(Default)]
struct ReadinessCache {
    checked_at: Option<tokio::time::Instant>,
    healthy: bool,
}

#[cfg(test)]
mod tests;
