use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[cfg(test)]
use std::path::Path;

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) const REQUEST_SPOOL_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct RequestSpoolAdmission {
    directory: Arc<PathBuf>,
    budget: Arc<RequestSpoolBudget>,
}

impl RequestSpoolAdmission {
    pub(crate) fn new(directory: PathBuf, limit_bytes: usize) -> Self {
        Self {
            directory: Arc::new(directory),
            budget: Arc::new(RequestSpoolBudget::new(limit_bytes)),
        }
    }

    pub(crate) async fn capture(
        &self,
        body: Body,
        maximum: usize,
        declared_content_length: Option<usize>,
    ) -> Result<RequestSpool, RequestSpoolCaptureError> {
        tokio::fs::create_dir_all(self.directory.as_ref())
            .await
            .map_err(|_| RequestSpoolCaptureError::Unavailable)?;
        let owner = tempfile::Builder::new()
            .prefix(".request-")
            .tempfile_in(self.directory.as_ref())
            .map_err(|_| RequestSpoolCaptureError::Unavailable)?;
        let writer = owner
            .reopen()
            .map(tokio::fs::File::from_std)
            .map_err(|_| RequestSpoolCaptureError::Unavailable)?;
        let mut capture = RequestSpoolCapture {
            owner: Some(owner),
            writer,
            lease: RequestSpoolLease::new(self.budget.clone()),
            length: 0,
            hasher: blake3::Hasher::new(),
        };
        if let Some(length) = declared_content_length {
            if length > maximum {
                return Err(RequestSpoolCaptureError::TooLarge);
            }
            capture
                .lease
                .try_grow(length)
                .ok_or(RequestSpoolCaptureError::CapacityExhausted)?;
        }

        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| RequestSpoolCaptureError::BodyReadRejected)?;
            for piece in chunk.chunks(REQUEST_SPOOL_CHUNK_BYTES) {
                let next = capture
                    .length
                    .checked_add(piece.len())
                    .ok_or(RequestSpoolCaptureError::TooLarge)?;
                if next > maximum {
                    return Err(RequestSpoolCaptureError::TooLarge);
                }
                let reserved = capture.lease.bytes();
                capture
                    .lease
                    .try_grow(next.saturating_sub(reserved))
                    .ok_or(RequestSpoolCaptureError::CapacityExhausted)?;
                capture
                    .writer
                    .write_all(piece)
                    .await
                    .map_err(|_| RequestSpoolCaptureError::Unavailable)?;
                capture.hasher.update(piece);
                capture.length = next;
            }
        }
        capture
            .writer
            .flush()
            .await
            .map_err(|_| RequestSpoolCaptureError::Unavailable)?;
        capture.lease.shrink_to(capture.length);
        let owner = capture
            .owner
            .take()
            .ok_or(RequestSpoolCaptureError::Unavailable)?;
        let digest = *capture.hasher.finalize().as_bytes();
        Ok(RequestSpool {
            owner,
            length: capture.length,
            digest,
            _lease: std::mem::replace(
                &mut capture.lease,
                RequestSpoolLease::new(self.budget.clone()),
            ),
        })
    }

    pub(crate) fn snapshot(&self) -> RequestSpoolSnapshot {
        self.budget.snapshot()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RequestSpoolSnapshot {
    pub(crate) used_bytes: usize,
    pub(crate) limit_bytes: usize,
    pub(crate) active_files: usize,
    pub(crate) capacity_rejections: u64,
}

struct RequestSpoolBudget {
    used_bytes: AtomicUsize,
    limit_bytes: usize,
    active_files: AtomicUsize,
    capacity_rejections: std::sync::atomic::AtomicU64,
}

impl RequestSpoolBudget {
    fn new(limit_bytes: usize) -> Self {
        Self {
            used_bytes: AtomicUsize::new(0),
            limit_bytes,
            active_files: AtomicUsize::new(0),
            capacity_rejections: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn try_grow(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let result = self
            .used_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|total| *total <= self.limit_bytes)
            });
        if result.is_err() {
            self.capacity_rejections.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn release(&self, bytes: usize) {
        let _ = self
            .used_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(bytes))
            });
    }

    fn snapshot(&self) -> RequestSpoolSnapshot {
        RequestSpoolSnapshot {
            used_bytes: self.used_bytes.load(Ordering::Acquire),
            limit_bytes: self.limit_bytes,
            active_files: self.active_files.load(Ordering::Acquire),
            capacity_rejections: self.capacity_rejections.load(Ordering::Relaxed),
        }
    }
}

struct RequestSpoolLease {
    budget: Arc<RequestSpoolBudget>,
    bytes: usize,
}

impl RequestSpoolLease {
    fn new(budget: Arc<RequestSpoolBudget>) -> Self {
        budget.active_files.fetch_add(1, Ordering::AcqRel);
        Self { budget, bytes: 0 }
    }

    fn bytes(&self) -> usize {
        self.bytes
    }

    fn try_grow(&mut self, bytes: usize) -> Option<()> {
        if self.budget.try_grow(bytes) {
            self.bytes = self.bytes.saturating_add(bytes);
            Some(())
        } else {
            None
        }
    }

    fn shrink_to(&mut self, bytes: usize) {
        let refund = self.bytes.saturating_sub(bytes);
        self.budget.release(refund);
        self.bytes = self.bytes.saturating_sub(refund);
    }
}

impl Drop for RequestSpoolLease {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
        self.budget.active_files.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RequestSpoolCapture {
    owner: Option<tempfile::NamedTempFile>,
    writer: tokio::fs::File,
    lease: RequestSpoolLease,
    length: usize,
    hasher: blake3::Hasher,
}

pub(crate) struct RequestSpool {
    owner: tempfile::NamedTempFile,
    length: usize,
    digest: [u8; 32],
    _lease: RequestSpoolLease,
}

impl RequestSpool {
    pub(crate) fn len(&self) -> usize {
        self.length
    }

    #[cfg(test)]
    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub(crate) async fn read_all(&self) -> Result<Bytes, RequestSpoolReadError> {
        let file = self.owner.reopen().map_err(|_| RequestSpoolReadError)?;
        let mut file = tokio::fs::File::from_std(file);
        let mut body = Vec::with_capacity(self.length);
        file.read_to_end(&mut body)
            .await
            .map_err(|_| RequestSpoolReadError)?;
        if body.len() != self.length || *blake3::hash(&body).as_bytes() != self.digest {
            return Err(RequestSpoolReadError);
        }
        Ok(Bytes::from(body))
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        self.owner.path()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestSpoolCaptureError {
    TooLarge,
    CapacityExhausted,
    BodyReadRejected,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestSpoolReadError;

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Arc};

    use futures_util::stream;

    use super::*;

    fn file_count(directory: &Path) -> usize {
        std::fs::read_dir(directory)
            .expect("request spool directory")
            .count()
    }

    #[tokio::test]
    async fn capture_round_trips_and_drop_unlinks_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned(), 256 * 1024);
        let body = Bytes::from(vec![b'x'; REQUEST_SPOOL_CHUNK_BYTES * 2 + 17]);
        let digest = *blake3::hash(&body).as_bytes();
        let spool = admission
            .capture(Body::from(body.clone()), body.len(), Some(body.len()))
            .await
            .unwrap();
        assert_eq!(spool.len(), body.len());
        assert_eq!(spool.digest(), digest);
        assert_eq!(spool.read_all().await.unwrap(), body);
        assert!(spool.path().exists());
        assert_eq!(file_count(directory.path()), 1);
        assert_eq!(admission.snapshot().used_bytes, body.len());
        drop(spool);
        assert_eq!(file_count(directory.path()), 0);
        assert_eq!(admission.snapshot().used_bytes, 0);
        assert_eq!(admission.snapshot().active_files, 0);
    }

    #[tokio::test]
    async fn declared_body_exhausts_disk_budget_before_polling() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned(), 64 * 1024);
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let body = Body::from_stream(stream::poll_fn(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Ready(Some(Ok::<_, Infallible>(Bytes::from_static(b"x"))))
        }));
        assert!(matches!(
            admission.capture(body, 128 * 1024, Some(128 * 1024)).await,
            Err(RequestSpoolCaptureError::CapacityExhausted)
        ));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(file_count(directory.path()), 0);
        assert_eq!(admission.snapshot().used_bytes, 0);
        assert_eq!(admission.snapshot().capacity_rejections, 1);
    }

    #[tokio::test]
    async fn cancelling_chunked_capture_releases_bytes_and_unlinks() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned(), 256 * 1024);
        let active = admission.clone();
        let body = Body::from_stream(stream::once(async {
            std::future::pending::<Result<Bytes, Infallible>>().await
        }));
        let capture = tokio::spawn(async move { active.capture(body, 256 * 1024, None).await });
        for _ in 0..100 {
            if admission.snapshot().active_files == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(admission.snapshot().active_files, 1);
        capture.abort();
        assert!(matches!(capture.await, Err(error) if error.is_cancelled()));
        assert_eq!(admission.snapshot().used_bytes, 0);
        assert_eq!(admission.snapshot().active_files, 0);
        assert_eq!(file_count(directory.path()), 0);
    }
}
