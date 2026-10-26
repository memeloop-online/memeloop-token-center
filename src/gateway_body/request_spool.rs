use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
};

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;

pub(crate) const REQUEST_SPOOL_CHUNK_BYTES: usize = 64 * 1024;

/// The background upload has not decided whether the object should stay.
const DURABLE_IN_FLIGHT: u8 = 0;
/// The object is the copy that outlives this process until the request ends.
const DURABLE_COMMITTED: u8 = 1;
/// The request finished or failed before the upload committed, so the writer deletes it.
const DURABLE_DROPPED: u8 = 2;

/// Request id that names the durable object. The id is already a server-generated
/// UUID; only that alphabet is accepted so the object key cannot escape the prefix.
#[derive(Clone, Debug)]
pub(crate) struct RequestBodyObjectKey(pub(crate) String);

pub(crate) fn request_body_object_key(request_id: &str) -> Option<String> {
    let safe = !request_id.is_empty()
        && request_id.len() <= 64
        && request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    safe.then(|| format!("gateway/request-bodies/{request_id}"))
}

#[derive(Clone)]
enum DurableRequestBodies {
    Archive(crate::archive::ArchiveStore),
    #[cfg(test)]
    Memory(Arc<MemoryObjectStore>),
}

#[cfg(test)]
pub(crate) struct MemoryObjectStore {
    objects: Mutex<std::collections::HashMap<String, Bytes>>,
    reads: AtomicUsize,
    writes: AtomicUsize,
    entered_writes: AtomicUsize,
    pauses: Mutex<std::collections::VecDeque<tokio::sync::oneshot::Receiver<()>>>,
}

#[cfg(test)]
impl MemoryObjectStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            objects: Mutex::new(std::collections::HashMap::new()),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            entered_writes: AtomicUsize::new(0),
            pauses: Mutex::new(std::collections::VecDeque::new()),
        })
    }

    fn pause_next_write(&self) -> tokio::sync::oneshot::Sender<()> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.pauses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(receiver);
        sender
    }
}

impl DurableRequestBodies {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), ()> {
        match self {
            Self::Archive(archive) => archive.put(key, body).await.map_err(|_| ()),
            #[cfg(test)]
            Self::Memory(store) => {
                let pause = store.pauses.lock().map_err(|_| ())?.pop_front();
                store.entered_writes.fetch_add(1, Ordering::AcqRel);
                if let Some(pause) = pause {
                    let _ = pause.await;
                }
                store.writes.fetch_add(1, Ordering::AcqRel);
                store
                    .objects
                    .lock()
                    .map_err(|_| ())?
                    .insert(key.to_owned(), body);
                Ok(())
            }
        }
    }

    async fn get(&self, key: &str, maximum: usize) -> Result<Bytes, ()> {
        match self {
            Self::Archive(archive) => archive
                .get_bounded(key, maximum.max(1))
                .await
                .map_err(|_| ()),
            #[cfg(test)]
            Self::Memory(store) => {
                store.reads.fetch_add(1, Ordering::AcqRel);
                store
                    .objects
                    .lock()
                    .map_err(|_| ())?
                    .get(key)
                    .cloned()
                    .ok_or(())
            }
        }
    }

    async fn delete(&self, key: &str) {
        match self {
            Self::Archive(archive) => {
                let _ = archive.delete(key).await;
            }
            #[cfg(test)]
            Self::Memory(store) => {
                if let Ok(mut objects) = store.objects.lock() {
                    objects.remove(key);
                }
            }
        }
    }
}

enum SpillSink {
    Archive(Box<crate::archive::ArchiveWriter>),
    #[cfg(test)]
    Memory(Vec<u8>),
}

struct CapturedBody {
    body: Option<Bytes>,
    length: usize,
    digest: [u8; 32],
    metadata: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    frames: Vec<Arc<tokio::sync::OwnedSemaphorePermit>>,
    lease: Arc<Mutex<RequestSpoolLease>>,
    durable: Option<DurableRequestBodies>,
    object_key: Option<String>,
    durable_settled: bool,
}

struct BodyResidency {
    frames: Mutex<Vec<Arc<tokio::sync::OwnedSemaphorePermit>>>,
    metadata: Mutex<Option<Arc<tokio::sync::OwnedSemaphorePermit>>>,
    memory_released: AtomicBool,
    durable_settled: AtomicBool,
}

impl BodyResidency {
    fn release_if_idle(&self) {
        if self.memory_released.load(Ordering::Acquire)
            && self.durable_settled.load(Ordering::Acquire)
        {
            if let Ok(mut frames) = self.frames.lock() {
                frames.clear();
            }
            if let Ok(mut metadata) = self.metadata.lock() {
                metadata.take();
            }
        }
    }
}

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

    /// Attach object storage before the admission is cloned into `AppState`.
    /// Memory remains the hot copy; the store is the copy that survives the process.
    pub(crate) fn with_archive(mut self, archive: crate::archive::ArchiveStore) -> Self {
        let usage = Arc::get_mut(&mut self.usage)
            .expect("durable store is attached before the admission is shared");
        usage.durable = Some(DurableRequestBodies::Archive(archive));
        self
    }

    #[cfg(test)]
    fn with_memory_store(mut self, store: Arc<MemoryObjectStore>) -> Self {
        let usage = Arc::get_mut(&mut self.usage)
            .expect("durable store is attached before the admission is shared");
        usage.durable = Some(DurableRequestBodies::Memory(store));
        self
    }

    #[cfg(test)]
    pub(crate) async fn capture(
        &self,
        body: Body,
        maximum: usize,
        declared_content_length: Option<usize>,
    ) -> Result<RequestSpool, RequestSpoolCaptureError> {
        self.capture_with_memory(body, maximum, declared_content_length, None, 0, None)
            .await
    }

    pub(crate) async fn capture_with_memory(
        &self,
        body: Body,
        maximum: usize,
        declared_content_length: Option<usize>,
        memory: Option<&Arc<super::memory::ProxyMemoryReservation>>,
        request_metadata_bytes: usize,
        body_key: Option<&str>,
    ) -> Result<RequestSpool, RequestSpoolCaptureError> {
        // The configured directory is for terminal journals, not request bodies.
        let _journal_directory = self.directory.as_path();
        let durable = self.usage.durable.clone();
        let object_key = durable.as_ref().and_then(|_| {
            request_body_object_key(body_key.unwrap_or(""))
                .or_else(|| request_body_object_key(&uuid::Uuid::now_v7().to_string()))
        });
        let metadata_bytes = request_metadata_bytes
            .saturating_add(std::mem::size_of::<RequestSpool>())
            .saturating_add(std::mem::size_of::<Mutex<RequestSpoolLease>>())
            .saturating_add(REQUEST_SPOOL_CHUNK_BYTES);
        let mut metadata = memory.and_then(|memory| {
            memory
                .try_reserve_ingress_bytes(metadata_bytes)
                .map(Arc::new)
        });
        while metadata.is_none() && memory.is_some() && evict_oldest_settled(&self.usage) {
            metadata = memory.and_then(|memory| {
                memory
                    .try_reserve_ingress_bytes(metadata_bytes)
                    .map(Arc::new)
            });
        }
        if memory.is_some() && metadata.is_none() && durable.is_none() {
            return Err(RequestSpoolCaptureError::MemoryExhausted);
        }
        let lease = Arc::new(Mutex::new(RequestSpoolLease::new(self.usage.clone())));
        if declared_content_length.is_some_and(|length| length > maximum) {
            return Err(RequestSpoolCaptureError::TooLarge);
        }
        // The body stays in this task. A blocking thread per chunk used to
        // exist for a local file, and it queued every upload behind the
        // blocking pool with no read deadline.
        lease
            .lock()
            .map_err(|_| RequestSpoolCaptureError::Unavailable)?
            .opened();
        let mut retained = Vec::new();
        let mut retained_frames = Vec::new();
        let mut length = 0_usize;
        let mut hasher = blake3::Hasher::new();
        let mut spilling = memory.is_some() && metadata.is_none() && durable.is_some();
        let mut sink = if spilling {
            let (store, key) = spill_target(&durable, &object_key)?;
            Some(open_spill(store, key).await?)
        } else {
            None
        };

        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    abort_spill(sink).await;
                    return Err(RequestSpoolCaptureError::BodyReadRejected);
                }
            };
            if chunk.len() > maximum.saturating_sub(length) {
                abort_spill(sink).await;
                return Err(RequestSpoolCaptureError::TooLarge);
            }
            let next = length
                .checked_add(chunk.len())
                .ok_or(RequestSpoolCaptureError::TooLarge)?;
            if next > maximum {
                abort_spill(sink).await;
                return Err(RequestSpoolCaptureError::TooLarge);
            }
            if !spilling {
                let mut frame_memory = memory
                    .and_then(|memory| memory.try_reserve_ingress_bytes(chunk.len()).map(Arc::new));
                while frame_memory.is_none()
                    && memory.is_some()
                    && evict_oldest_settled(&self.usage)
                {
                    frame_memory = memory.and_then(|memory| {
                        memory.try_reserve_ingress_bytes(chunk.len()).map(Arc::new)
                    });
                }
                if frame_memory.is_none() && memory.is_some() {
                    let Some(store) = durable.as_ref() else {
                        return Err(RequestSpoolCaptureError::MemoryExhausted);
                    };
                    let Some(key) = object_key.as_deref() else {
                        return Err(RequestSpoolCaptureError::Unavailable);
                    };
                    spilling = true;
                    let prior = Bytes::from(std::mem::take(&mut retained));
                    retained_frames.clear();
                    metadata.take();
                    if let Ok(mut lease) = lease.lock() {
                        lease.release_all();
                    }
                    sink = Some(open_spill(store, key).await?);
                    if let Some(sink) = sink.as_mut() {
                        write_spill(sink, prior).await?;
                        write_spill(sink, chunk.clone()).await?;
                    }
                } else {
                    if let Some(permit) = frame_memory {
                        retained_frames.push(permit);
                    }
                    lease
                        .lock()
                        .map_err(|_| RequestSpoolCaptureError::Unavailable)?
                        .record_written(chunk.len());
                    retained.extend_from_slice(&chunk);
                }
            } else if let Some(sink) = sink.as_mut() {
                write_spill(sink, chunk.clone()).await?;
            }
            hasher.update(&chunk);
            length = next;
        }
        let digest = *hasher.finalize().as_bytes();
        if spilling {
            let (store, key) = spill_target(&durable, &object_key)?;
            let Some(sink) = sink.take() else {
                return Err(RequestSpoolCaptureError::Unavailable);
            };
            finish_spill(store, key, sink).await?;
            return Ok(self.finished_spool(CapturedBody {
                body: None,
                length,
                digest,
                metadata: None,
                frames: Vec::new(),
                lease,
                durable,
                object_key,
                durable_settled: true,
            }));
        }
        let retained = Bytes::from(retained);
        let publish = durable.is_some() && object_key.is_some();
        Ok(self
            .finished_spool(CapturedBody {
                body: Some(retained),
                length,
                digest,
                metadata,
                frames: retained_frames,
                lease,
                durable,
                object_key,
                durable_settled: !publish,
            })
            .arm())
    }

    fn finished_spool(&self, captured: CapturedBody) -> RequestSpool {
        RequestSpool {
            shared: Arc::new(SharedBody {
                body: Mutex::new(captured.body),
                length: captured.length,
                digest: captured.digest,
                residency: Arc::new(BodyResidency {
                    frames: Mutex::new(captured.frames),
                    metadata: Mutex::new(captured.metadata),
                    memory_released: AtomicBool::new(false),
                    durable_settled: AtomicBool::new(captured.durable_settled),
                }),
                lease: captured.lease,
                durable: captured.durable,
                object_key: captured.object_key,
                durable_ok: Arc::new(AtomicBool::new(captured.durable_settled)),
                durable_phase: Arc::new(AtomicU8::new(if captured.durable_settled {
                    DURABLE_COMMITTED
                } else {
                    DURABLE_IN_FLIGHT
                })),
                durable_write: Mutex::new(None),
            }),
            #[cfg(test)]
            read_count: self.read_count.clone(),
        }
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
    durable: Option<DurableRequestBodies>,
    /// Oldest resident first. Only a settled object-storage copy may leave memory.
    residents: Mutex<std::collections::VecDeque<std::sync::Weak<SharedBody>>>,
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

struct SharedBody {
    body: Mutex<Option<Bytes>>,
    length: usize,
    digest: [u8; 32],
    residency: Arc<BodyResidency>,
    lease: Arc<Mutex<RequestSpoolLease>>,
    durable: Option<DurableRequestBodies>,
    object_key: Option<String>,
    durable_ok: Arc<AtomicBool>,
    durable_phase: Arc<AtomicU8>,
    durable_write: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

pub(crate) struct RequestSpool {
    shared: Arc<SharedBody>,
    #[cfg(test)]
    read_count: Arc<AtomicUsize>,
}

impl RequestSpool {
    pub(crate) fn len(&self) -> usize {
        self.shared.length
    }

    #[cfg(test)]
    pub(crate) fn digest(&self) -> [u8; 32] {
        self.shared.digest
    }

    #[cfg(test)]
    fn durable_settled_for_test(&self) -> bool {
        self.shared
            .residency
            .durable_settled
            .load(Ordering::Acquire)
    }

    fn arm(self) -> Self {
        let publish = self.shared.durable.is_some()
            && self.shared.object_key.is_some()
            && !self
                .shared
                .residency
                .durable_settled
                .load(Ordering::Acquire);
        if publish {
            publish_durable(&self.shared);
            if let Ok(lease) = self.shared.lease.lock()
                && let Ok(mut residents) = lease.usage.residents.lock()
            {
                residents.push_back(Arc::downgrade(&self.shared));
            }
        }
        self
    }

    /// Drop the in-memory copy. A settled object, when one was written, stays
    /// until this spool is dropped so a replacement pod can still read it.
    pub(crate) fn release_retained(&self) {
        release_resident_memory(&self.shared);
    }

    pub(crate) async fn read_all(&self) -> Result<Bytes, RequestSpoolReadError> {
        #[cfg(test)]
        self.read_count.fetch_add(1, Ordering::AcqRel);
        let resident = self
            .shared
            .body
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(body) = resident {
            return verified_body(&self.shared, body);
        }
        let handle = self
            .shared
            .durable_write
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        if !self.shared.durable_ok.load(Ordering::Acquire) {
            return Err(RequestSpoolReadError);
        }
        let Some(store) = self.shared.durable.as_ref() else {
            return Err(RequestSpoolReadError);
        };
        let Some(key) = self.shared.object_key.as_deref() else {
            return Err(RequestSpoolReadError);
        };
        let body = store
            .get(key, self.shared.length)
            .await
            .map_err(|_| RequestSpoolReadError)?;
        verified_body(&self.shared, body)
    }
}

impl Drop for RequestSpool {
    fn drop(&mut self) {
        release_resident_memory(&self.shared);
        // Detach the writer. Aborting it lets a late PutObject land after the
        // delete and leave a body no request still owns.
        drop(
            self.shared
                .durable_write
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take(),
        );
        let claimed = self.shared.durable_phase.compare_exchange(
            DURABLE_IN_FLIGHT,
            DURABLE_DROPPED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if !matches!(claimed, Err(DURABLE_COMMITTED)) {
            return;
        }
        let Some(store) = self.shared.durable.clone() else {
            return;
        };
        let Some(key) = self.shared.object_key.clone() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                store.delete(&key).await;
            });
        }
    }
}

fn verified_body(shared: &SharedBody, body: Bytes) -> Result<Bytes, RequestSpoolReadError> {
    if body.len() != shared.length || *blake3::hash(&body).as_bytes() != shared.digest {
        return Err(RequestSpoolReadError);
    }
    Ok(body)
}

fn release_resident_memory(shared: &SharedBody) {
    let removed = shared
        .body
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .is_some();
    if removed && let Ok(mut lease) = shared.lease.lock() {
        lease.release_all();
    }
    shared
        .residency
        .memory_released
        .store(true, Ordering::Release);
    shared.residency.release_if_idle();
}

fn evict_oldest_settled(usage: &RequestSpoolUsage) -> bool {
    let Ok(mut residents) = usage.residents.lock() else {
        return false;
    };
    while let Some(resident) = residents.pop_front() {
        let Some(shared) = resident.upgrade() else {
            continue;
        };
        let still_resident = shared
            .body
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some();
        if !still_resident {
            continue;
        }
        if !shared.residency.durable_settled.load(Ordering::Acquire) {
            residents.push_front(Arc::downgrade(&shared));
            return false;
        }
        release_resident_memory(&shared);
        return true;
    }
    false
}

fn publish_durable(shared: &SharedBody) {
    let Some(store) = shared.durable.clone() else {
        return;
    };
    let Some(key) = shared.object_key.clone() else {
        return;
    };
    let Some(bytes) = shared
        .body
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
    else {
        return;
    };
    let ok = Arc::clone(&shared.durable_ok);
    let phase = Arc::clone(&shared.durable_phase);
    let residency = shared.residency.clone();
    let handle = tokio::spawn(async move {
        let wrote = store.put(&key, bytes).await.is_ok();
        if wrote {
            match phase.compare_exchange(
                DURABLE_IN_FLIGHT,
                DURABLE_COMMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    ok.store(true, Ordering::Release);
                    // Only a completed upload may leave memory. A failed put
                    // keeps this resident as the only copy.
                    residency.durable_settled.store(true, Ordering::Release);
                }
                Err(DURABLE_DROPPED) => {
                    store.delete(&key).await;
                    residency.durable_settled.store(true, Ordering::Release);
                }
                Err(_) => {}
            }
        } else {
            tracing::warn!("request body stayed in memory; object storage write failed");
        }
        residency.release_if_idle();
    });
    *shared
        .durable_write
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(handle);
}

fn spill_target<'a>(
    durable: &'a Option<DurableRequestBodies>,
    object_key: &'a Option<String>,
) -> Result<(&'a DurableRequestBodies, &'a str), RequestSpoolCaptureError> {
    match (durable.as_ref(), object_key.as_deref()) {
        (Some(store), Some(key)) => Ok((store, key)),
        _ => Err(RequestSpoolCaptureError::Unavailable),
    }
}

async fn open_spill(
    durable: &DurableRequestBodies,
    key: &str,
) -> Result<SpillSink, RequestSpoolCaptureError> {
    match durable {
        DurableRequestBodies::Archive(archive) => archive
            .start_writer(key)
            .await
            .map(|writer| SpillSink::Archive(Box::new(writer)))
            .map_err(|_| RequestSpoolCaptureError::Unavailable),
        #[cfg(test)]
        DurableRequestBodies::Memory(_) => Ok(SpillSink::Memory(Vec::new())),
    }
}

async fn write_spill(sink: &mut SpillSink, bytes: Bytes) -> Result<(), RequestSpoolCaptureError> {
    match sink {
        SpillSink::Archive(writer) => writer
            .write(bytes)
            .await
            .map_err(|_| RequestSpoolCaptureError::Unavailable),
        #[cfg(test)]
        SpillSink::Memory(buffered) => {
            buffered.extend_from_slice(&bytes);
            Ok(())
        }
    }
}

async fn finish_spill(
    durable: &DurableRequestBodies,
    key: &str,
    sink: SpillSink,
) -> Result<(), RequestSpoolCaptureError> {
    match sink {
        SpillSink::Archive(writer) => {
            let _staged = (*writer)
                .finish_staged()
                .await
                .map_err(|_| RequestSpoolCaptureError::Unavailable)?;
            let _ = (durable, key);
            Ok(())
        }
        #[cfg(test)]
        SpillSink::Memory(buffered) => durable
            .put(key, Bytes::from(buffered))
            .await
            .map_err(|_| RequestSpoolCaptureError::Unavailable),
    }
}

async fn abort_spill(sink: Option<SpillSink>) {
    if let Some(SpillSink::Archive(writer)) = sink {
        let _ = (*writer).abort().await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestSpoolCaptureError {
    TooLarge,
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

    #[tokio::test]
    async fn hot_body_is_served_from_memory_and_written_through() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let body = Bytes::from(vec![b'h'; 32 * 1024]);
        let spool = admission
            .capture_with_memory(
                Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                None,
                0,
                Some("11111111-1111-4111-8111-111111111111"),
            )
            .await
            .unwrap();
        assert_eq!(spool.read_all().await.unwrap(), body);
        assert_eq!(store.reads.load(Ordering::Acquire), 0);
        let key = request_body_object_key("11111111-1111-4111-8111-111111111111").unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while store.writes.load(Ordering::Acquire) == 0 {
            if tokio::time::Instant::now() > deadline {
                panic!("durable write did not finish");
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            store.objects.lock().unwrap().get(&key).cloned(),
            Some(body.clone())
        );
        spool.release_retained();
        assert_eq!(spool.read_all().await.unwrap(), body);
        assert_eq!(store.reads.load(Ordering::Acquire), 1);
        assert_eq!(file_count(directory.path()), 0);
    }

    #[tokio::test]
    async fn full_memory_evicts_the_oldest_settled_body_before_spilling() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(256 * 1024);
        let memory = budget.reservation();
        let first = Bytes::from(vec![b'a'; 128 * 1024]);
        let oldest = admission
            .capture_with_memory(
                Body::from(first.clone()),
                first.len(),
                Some(first.len()),
                Some(&memory),
                0,
                Some("oldest-body"),
            )
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !oldest.durable_settled_for_test() {
            if tokio::time::Instant::now() > deadline {
                panic!("oldest body was not durable");
            }
            tokio::task::yield_now().await;
        }
        let second = Bytes::from(vec![b'b'; 128 * 1024]);
        let newest = admission
            .capture_with_memory(
                Body::from(second.clone()),
                second.len(),
                Some(second.len()),
                Some(&memory),
                0,
                Some("newest-body"),
            )
            .await
            .unwrap();
        assert_eq!(newest.read_all().await.unwrap(), second);
        assert_eq!(
            store.reads.load(Ordering::Acquire),
            0,
            "the newest body stays in memory"
        );
        assert_eq!(oldest.read_all().await.unwrap(), first);
        assert_eq!(store.reads.load(Ordering::Acquire), 1);
        assert_eq!(file_count(directory.path()), 0);
    }

    #[tokio::test]
    async fn finished_request_deletes_its_object_and_a_dropped_upload_does_not_remain() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let body = Bytes::from_static(b"kept-until-the-response-ends");
        let key = request_body_object_key("22222222-2222-4222-8222-222222222222").unwrap();
        let spool = admission
            .capture_with_memory(
                Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                None,
                0,
                Some("22222222-2222-4222-8222-222222222222"),
            )
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !spool.durable_settled_for_test() {
            if tokio::time::Instant::now() > deadline {
                panic!("upload did not settle");
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(store.objects.lock().unwrap().get(&key).cloned(), Some(body));
        drop(spool);
        while store.objects.lock().unwrap().contains_key(&key) {
            if tokio::time::Instant::now() > deadline {
                panic!("finished request left its object behind");
            }
            tokio::task::yield_now().await;
        }

        let release = store.pause_next_write();
        let abandoned = Bytes::from_static(b"abandoned-during-upload");
        let abandoned_key =
            request_body_object_key("33333333-3333-4333-8333-333333333333").unwrap();
        let spool = admission
            .capture_with_memory(
                Body::from(abandoned.clone()),
                abandoned.len(),
                Some(abandoned.len()),
                None,
                0,
                Some("33333333-3333-4333-8333-333333333333"),
            )
            .await
            .unwrap();
        while store.entered_writes.load(Ordering::Acquire) < 2 {
            if tokio::time::Instant::now() > deadline {
                panic!("second upload did not start");
            }
            tokio::task::yield_now().await;
        }
        drop(spool);
        let _ = release.send(());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while store.objects.lock().unwrap().contains_key(&abandoned_key)
            || store.writes.load(Ordering::Acquire) < 2
        {
            if tokio::time::Instant::now() > deadline {
                panic!("dropped upload left an object behind");
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn unsettled_body_stays_resident_and_the_new_upload_spills() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let release = store.pause_next_write();
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(256 * 1024);
        let memory = budget.reservation();
        let first = Bytes::from(vec![b'a'; 128 * 1024]);
        let oldest = admission
            .capture_with_memory(
                Body::from(first.clone()),
                first.len(),
                Some(first.len()),
                Some(&memory),
                0,
                Some("oldest-unsettled"),
            )
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while store.entered_writes.load(Ordering::Acquire) == 0 {
            if tokio::time::Instant::now() > deadline {
                panic!("oldest upload did not start");
            }
            tokio::task::yield_now().await;
        }
        assert!(!oldest.durable_settled_for_test());
        let second = Bytes::from(vec![b'b'; 128 * 1024]);
        let newest = admission
            .capture_with_memory(
                Body::from(second.clone()),
                second.len(),
                Some(second.len()),
                Some(&memory),
                0,
                Some("newest-spilled"),
            )
            .await
            .unwrap();
        assert_eq!(newest.read_all().await.unwrap(), second);
        let reads_after_spill = store.reads.load(Ordering::Acquire);
        assert!(
            reads_after_spill >= 1,
            "the new body was not kept in memory"
        );
        assert_eq!(oldest.read_all().await.unwrap(), first);
        assert_eq!(store.reads.load(Ordering::Acquire), reads_after_spill);
        let _ = release.send(());
        while !oldest.durable_settled_for_test() {
            if tokio::time::Instant::now() > deadline {
                panic!("oldest upload did not settle after release");
            }
            tokio::task::yield_now().await;
        }
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
