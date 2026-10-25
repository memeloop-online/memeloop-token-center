use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;

pub(crate) const REQUEST_SPOOL_CHUNK_BYTES: usize = 64 * 1024;

#[cfg(test)]
mod ownership_tests;

#[derive(Clone)]
pub(crate) struct RequestSpoolAdmission {
    directory: Arc<PathBuf>,
    usage: Arc<RequestSpoolUsage>,
    #[cfg(test)]
    read_count: Arc<AtomicUsize>,
}

impl RequestSpoolAdmission {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory: Arc::new(directory),
            usage: Arc::new(RequestSpoolUsage::default()),
            #[cfg(test)]
            read_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    pub(crate) async fn capture(
        &self,
        body: Body,
        maximum: usize,
        declared_content_length: Option<usize>,
    ) -> Result<RequestSpool, RequestSpoolCaptureError> {
        self.capture_with_memory(body, maximum, declared_content_length, None, 0)
            .await
    }

    pub(crate) async fn capture_with_memory(
        &self,
        body: Body,
        maximum: usize,
        declared_content_length: Option<usize>,
        memory: Option<&Arc<super::memory::ProxyMemoryReservation>>,
        request_metadata_bytes: usize,
    ) -> Result<RequestSpool, RequestSpoolCaptureError> {
        // The configured directory is for terminal journals, not request bodies.
        let _journal_directory = self.directory.as_path();
        let metadata = memory
            .map(|memory| {
                memory
                    .try_reserve_ingress_bytes(
                        request_metadata_bytes
                            .saturating_add(std::mem::size_of::<RequestSpool>())
                            .saturating_add(std::mem::size_of::<Mutex<RequestSpoolLease>>())
                            .saturating_add(REQUEST_SPOOL_CHUNK_BYTES),
                    )
                    .map(Arc::new)
                    .ok_or(RequestSpoolCaptureError::MemoryExhausted)
            })
            .transpose()?;
        let lease = Arc::new(Mutex::new(RequestSpoolLease::new(self.usage.clone())));
        if declared_content_length.is_some_and(|length| length > maximum) {
            return Err(RequestSpoolCaptureError::TooLarge);
        }
        let opening_metadata = metadata.clone();
        let opening_lease = lease.clone();
        #[cfg(test)]
        let blocking_open = self.usage.blocking_open.clone();
        let opening = tokio::task::spawn_blocking(move || {
            let _opening_metadata = opening_metadata;
            opening_lease
                .lock()
                .map_err(|_| io::Error::other("request spool ownership unavailable"))?
                .opened();
            #[cfg(test)]
            if let Some(pause) = blocking_open {
                pause.block();
            }
            Ok::<_, io::Error>(())
        });
        #[cfg(test)]
        if let Some(pause) = &self.usage.blocking_open {
            pause.record(&opening);
        }
        opening
            .await
            .map_err(|_| RequestSpoolCaptureError::Unavailable)?
            .map_err(spool_io_error)?;
        let mut retained = Vec::new();
        let mut length = 0_usize;
        let mut hasher = blake3::Hasher::new();

        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| RequestSpoolCaptureError::BodyReadRejected)?;
            if chunk.len() > maximum.saturating_sub(length) {
                return Err(RequestSpoolCaptureError::TooLarge);
            }
            let frame_memory = memory
                .map(|memory| {
                    memory
                        .try_reserve_ingress_bytes(chunk.len())
                        .map(Arc::new)
                        .ok_or(RequestSpoolCaptureError::MemoryExhausted)
                })
                .transpose()?;
            for offset in (0..chunk.len()).step_by(REQUEST_SPOOL_CHUNK_BYTES) {
                let end = (offset + REQUEST_SPOOL_CHUNK_BYTES).min(chunk.len());
                let next = length
                    .checked_add(end - offset)
                    .ok_or(RequestSpoolCaptureError::TooLarge)?;
                if next > maximum {
                    return Err(RequestSpoolCaptureError::TooLarge);
                }
                let write_metadata = metadata.clone();
                let write_frame = frame_memory.clone();
                let write_lease = lease.clone();
                let write_bytes = chunk.slice(offset..end);
                #[cfg(test)]
                let blocking_write = self.usage.blocking_write.clone();
                let writing = tokio::task::spawn_blocking(move || {
                    let _write_metadata = write_metadata;
                    let _write_frame = write_frame;
                    #[cfg(test)]
                    if let Some(pause) = blocking_write {
                        pause.block();
                    }
                    write_lease
                        .lock()
                        .map_err(|_| io::Error::other("request spool ownership unavailable"))?
                        .record_written(write_bytes.len());
                    Ok::<_, io::Error>(write_bytes)
                });
                #[cfg(test)]
                if let Some(pause) = &self.usage.blocking_write {
                    pause.record(&writing);
                }
                let piece = writing
                    .await
                    .map_err(|_| RequestSpoolCaptureError::Unavailable)?
                    .map_err(spool_io_error)?;
                hasher.update(&piece);
                retained.extend_from_slice(&piece);
                length = next;
            }
        }
        let digest = *hasher.finalize().as_bytes();
        Ok(RequestSpool {
            body: Mutex::new(Some(Bytes::from(retained))),
            length,
            digest,
            _metadata: metadata,
            lease,
            #[cfg(test)]
            read_count: self.read_count.clone(),
        })
    }

    pub(crate) fn snapshot(&self) -> RequestSpoolSnapshot {
        self.usage.snapshot()
    }

    #[cfg(test)]
    pub(crate) fn read_count_for_test(&self) -> usize {
        self.read_count.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RequestSpoolSnapshot {
    pub(crate) used_bytes: usize,
    pub(crate) active_files: usize,
}

#[derive(Default)]
struct RequestSpoolUsage {
    used_bytes: AtomicUsize,
    active_files: AtomicUsize,
    #[cfg(test)]
    blocking_open: Option<Arc<ownership_tests::BlockingPause>>,
    #[cfg(test)]
    blocking_write: Option<Arc<ownership_tests::BlockingPause>>,
}

impl RequestSpoolUsage {
    fn snapshot(&self) -> RequestSpoolSnapshot {
        RequestSpoolSnapshot {
            used_bytes: self.used_bytes.load(Ordering::Acquire),
            active_files: self.active_files.load(Ordering::Acquire),
        }
    }
}

struct RequestSpoolLease {
    usage: Arc<RequestSpoolUsage>,
    bytes: usize,
    opened: bool,
}

impl RequestSpoolLease {
    fn new(usage: Arc<RequestSpoolUsage>) -> Self {
        Self {
            usage,
            bytes: 0,
            opened: false,
        }
    }

    fn opened(&mut self) {
        self.usage.active_files.fetch_add(1, Ordering::AcqRel);
        self.opened = true;
    }

    fn record_written(&mut self, bytes: usize) {
        self.usage.used_bytes.fetch_add(bytes, Ordering::AcqRel);
        self.bytes += bytes;
    }

    fn release_all(&mut self) {
        self.usage
            .used_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
        self.bytes = 0;
    }
}

impl Drop for RequestSpoolLease {
    fn drop(&mut self) {
        self.usage
            .used_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
        if self.opened {
            self.usage.active_files.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

pub(crate) struct RequestSpool {
    body: Mutex<Option<Bytes>>,
    length: usize,
    digest: [u8; 32],
    _metadata: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    lease: Arc<Mutex<RequestSpoolLease>>,
    #[cfg(test)]
    read_count: Arc<AtomicUsize>,
}

impl RequestSpool {
    pub(crate) fn len(&self) -> usize {
        self.length
    }

    #[cfg(test)]
    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Drop the retained upload once it has been handed to the upstream and the
    /// first response byte has arrived. Later conversation projection uses the
    /// semantic JSON already parsed from this buffer, not a second copy.
    pub(crate) fn release_retained(&self) {
        let mut stored = self.body.lock().unwrap_or_else(|error| error.into_inner());
        if stored.take().is_some()
            && let Ok(mut lease) = self.lease.lock()
        {
            lease.release_all();
        }
    }

    pub(crate) async fn read_all(&self) -> Result<Bytes, RequestSpoolReadError> {
        #[cfg(test)]
        self.read_count.fetch_add(1, Ordering::AcqRel);
        let body = self
            .body
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or(RequestSpoolReadError)?;
        if body.len() != self.length || *blake3::hash(&body).as_bytes() != self.digest {
            return Err(RequestSpoolReadError);
        }
        Ok(body)
    }
}

fn storage_exhausted(code: i32) -> bool {
    #[cfg(unix)]
    {
        code == rustix::io::Errno::NOSPC.raw_os_error()
            || code == rustix::io::Errno::DQUOT.raw_os_error()
    }
    #[cfg(not(unix))]
    {
        let _ = code;
        false
    }
}

fn spool_io_error(error: io::Error) -> RequestSpoolCaptureError {
    tracing::error!(
        error_kind = ?error.kind(),
        os_error = error.raw_os_error(),
        "local request spool I/O unavailable"
    );
    match error.raw_os_error() {
        Some(code) if storage_exhausted(code) => RequestSpoolCaptureError::StorageFull,
        _ => RequestSpoolCaptureError::Unavailable,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestSpoolCaptureError {
    TooLarge,
    StorageFull,
    MemoryExhausted,
    BodyReadRejected,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestSpoolReadError;

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, path::Path, sync::Arc};

    use futures_util::stream;

    use super::*;

    fn file_count(directory: &Path) -> usize {
        std::fs::read_dir(directory).map_or(0, |entries| entries.count())
    }

    #[tokio::test]
    async fn capture_round_trips_and_drop_unlinks_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned());
        let body = Bytes::from(vec![b'x'; REQUEST_SPOOL_CHUNK_BYTES * 2 + 17]);
        let digest = *blake3::hash(&body).as_bytes();
        let spool = admission
            .capture(Body::from(body.clone()), body.len(), Some(body.len()))
            .await
            .unwrap();
        assert_eq!(spool.len(), body.len());
        assert_eq!(spool.digest(), digest);
        assert_eq!(spool.read_all().await.unwrap(), body);
        assert_eq!(file_count(directory.path()), 0);
        spool.release_retained();
        assert_eq!(admission.snapshot().used_bytes, 0);
        assert!(spool.read_all().await.is_err());
        assert_eq!(spool.len(), body.len());
        drop(spool);
        assert_eq!(file_count(directory.path()), 0);
        assert_eq!(admission.snapshot().used_bytes, 0);
        assert_eq!(admission.snapshot().active_files, 0);
    }

    #[tokio::test]
    async fn oversized_declared_body_is_rejected_before_polling_or_file_creation() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned());
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let body = Body::from_stream(stream::poll_fn(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Ready(Some(Ok::<_, Infallible>(Bytes::from_static(b"x"))))
        }));
        assert!(matches!(
            admission.capture(body, 64 * 1024, Some(128 * 1024)).await,
            Err(RequestSpoolCaptureError::TooLarge)
        ));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(file_count(directory.path()), 0);
        assert_eq!(admission.snapshot().used_bytes, 0);
        assert_eq!(admission.snapshot().active_files, 0);
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_full_and_quota_errors_are_distinct_from_unavailable() {
        for errno in [rustix::io::Errno::NOSPC, rustix::io::Errno::DQUOT] {
            assert_eq!(
                spool_io_error(io::Error::from_raw_os_error(errno.raw_os_error())),
                RequestSpoolCaptureError::StorageFull
            );
        }
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotFound] {
            assert_eq!(
                spool_io_error(io::Error::from(kind)),
                RequestSpoolCaptureError::Unavailable
            );
        }
    }

    #[tokio::test]
    async fn cancelling_chunked_capture_releases_bytes_and_unlinks() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned());
        let active = admission.clone();
        let (second_chunk_polled, second_chunk_poll) = tokio::sync::oneshot::channel();
        let body = Body::from_stream(
            stream::once(async {
                Ok::<_, Infallible>(Bytes::from(vec![b'x'; REQUEST_SPOOL_CHUNK_BYTES]))
            })
            .chain(stream::once(async move {
                let _ = second_chunk_polled.send(());
                std::future::pending::<Result<Bytes, Infallible>>().await
            })),
        );
        let capture = tokio::spawn(async move { active.capture(body, 256 * 1024, None).await });
        second_chunk_poll
            .await
            .expect("capture polls the next chunk only after writing the first");
        assert_eq!(admission.snapshot().active_files, 1);
        assert_eq!(admission.snapshot().used_bytes, REQUEST_SPOOL_CHUNK_BYTES);
        capture.abort();
        assert!(matches!(capture.await, Err(error) if error.is_cancelled()));
        assert_eq!(admission.snapshot().used_bytes, 0);
        assert_eq!(admission.snapshot().active_files, 0);
        assert_eq!(file_count(directory.path()), 0);
    }

    #[tokio::test]
    async fn anonymous_spool_leaves_no_restart_orphan() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("request-spool");
        let admission = RequestSpoolAdmission::new(path.clone());
        let spool = admission
            .capture(Body::from("sensitive"), 1024, None)
            .await
            .unwrap();
        assert_eq!(file_count(&path), 0, "active spool must be anonymous");
        drop(spool);
        drop(admission);
        let restarted = RequestSpoolAdmission::new(path.clone());
        assert_eq!(restarted.snapshot().used_bytes, 0);
        assert_eq!(file_count(&path), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn memory_capture_does_not_follow_a_spool_directory_symlink() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = parent.path().join("request-spool");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let admission = RequestSpoolAdmission::new(link);
        let spool = admission
            .capture(Body::from("secret"), 1024, None)
            .await
            .unwrap();
        assert_eq!(spool.read_all().await.unwrap(), Bytes::from("secret"));
        assert_eq!(file_count(&target), 0);
    }
}
