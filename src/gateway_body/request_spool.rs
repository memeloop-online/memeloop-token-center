use super::carrier::{BodyCarrier, CarrierCapture};
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
    safe.then(|| match uuid::Uuid::parse_str(request_id) {
        Ok(request_id) => format!("staging/proxy/{request_id}/request/{request_id}/body.json"),
        Err(_) => format!("gateway/request-bodies/{request_id}"),
    })
}

pub(crate) fn request_body_staging_key(
    locator: &str,
) -> Option<crate::archive_staging::ArchiveStagingKey> {
    use crate::archive_staging::{ArchiveStagingKey, ArchiveStagingOwner, ArchiveStagingPurpose};
    let (identity, bare) = match locator.strip_prefix("stores/staging/") {
        Some(qualified) => {
            let (identity, bare) = qualified.split_once('/')?;
            (Some(identity), bare)
        }
        None => (None, locator),
    };
    let request_id = bare.strip_prefix("staging/proxy/")?.split('/').next()?;
    let request_id = uuid::Uuid::parse_str(request_id).ok()?;
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(request_id),
        ArchiveStagingPurpose::Request,
        request_id,
    )
    .ok()?
    .with_backend_identity(identity)
    .ok()?;
    (format!("{}/body.json", key.canonical_prefix()) == locator).then_some(key)
}

pub(crate) fn source_request_id(locator: &str) -> Option<uuid::Uuid> {
    request_body_staging_key(locator).map(|key| key.owner.id())
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
    fail_writes: AtomicBool,
    pauses: Mutex<std::collections::VecDeque<tokio::sync::oneshot::Receiver<()>>>,
    backing: Mutex<Option<crate::archive::ArchiveStore>>,
}

#[cfg(test)]
impl MemoryObjectStore {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            objects: Mutex::new(std::collections::HashMap::new()),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            entered_writes: AtomicUsize::new(0),
            fail_writes: AtomicBool::new(false),
            pauses: Mutex::new(std::collections::VecDeque::new()),
            backing: Mutex::new(None),
        })
    }

    pub(crate) fn pause_next_write(&self) -> tokio::sync::oneshot::Sender<()> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.pauses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(receiver);
        sender
    }

    pub(crate) fn fail_writes_for_test(&self) {
        self.fail_writes.store(true, Ordering::Release);
    }

    pub(crate) fn resume_writes_for_test(&self, archive: crate::archive::ArchiveStore) {
        *self.backing.lock().unwrap() = Some(archive);
        self.fail_writes.store(false, Ordering::Release);
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
                if store.fail_writes.load(Ordering::Acquire) {
                    return Err(());
                }
                let backing = store.backing.lock().map_err(|_| ())?.clone();
                if let Some(backing) = backing {
                    backing.put(key, body.clone()).await.map_err(|_| ())?;
                }
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

    async fn put_carrier(
        &self,
        key: &str,
        source: &Arc<BodyCarrier>,
        upload: &crate::api::proxy::persistence::Persistence,
    ) -> Result<(), ()> {
        match self {
            Self::Archive(archive) => {
                // Multipart owns queued buffers until a >=5MiB part completes.
                // Its existing independent upload envelope must own those
                // copies; retaining the source pool's replay lease here would
                // wait on the very bytes queued in the multipart writer.
                let _upload_memory = upload
                    .stream_memory
                    .temporary_wait(
                        crate::archive::ARCHIVE_MULTIPART_PART_BYTES
                            + 5 * REQUEST_SPOOL_CHUNK_BYTES,
                        super::memory::CAPTURE_MEMORY_WEIGHT,
                    )
                    .await
                    .ok_or(())?;
                let mut writer = archive.start_writer(key).await.map_err(|_| ())?;
                let mut stream = source.open_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(_) => {
                            let _ = writer.abort().await;
                            return Err(());
                        }
                    };
                    let upload_bytes = Bytes::copy_from_slice(&chunk);
                    drop(chunk);
                    if writer.write(upload_bytes).await.is_err() {
                        let _ = writer.abort().await;
                        return Err(());
                    }
                }
                let result = writer.finish_staged().await.map_err(|_| ())?;
                if result.size_bytes != source.len() || result.blake3_digest != source.digest() {
                    return Err(());
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(_) => {
                let mut bytes = Vec::new();
                let mut stream = source.open_stream();
                while let Some(chunk) = stream.next().await {
                    bytes.extend_from_slice(&chunk.map_err(|_| ())?);
                }
                self.put(key, Bytes::from(bytes)).await
            }
        }
    }

    async fn delete(&self, key: &str) {
        match self {
            Self::Archive(archive) => {
                if key.starts_with("stores/staging/") {
                    if let Some(typed) = request_body_staging_key(key) {
                        let _ = archive.delete_archive_staging_object(typed, key).await;
                    }
                } else {
                    let _ = archive.delete(key).await;
                }
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

struct CapturedBody {
    body: Option<Bytes>,
    carrier: Option<Arc<BodyCarrier>>,
    length: usize,
    json_nodes: usize,
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
    carrier_backed: bool,
}

impl BodyResidency {
    fn release_if_idle(&self) {
        if self.memory_released.load(Ordering::Acquire)
            && (self.carrier_backed || self.durable_settled.load(Ordering::Acquire))
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
    usage: Arc<RequestSpoolUsage>,
    #[cfg(test)]
    read_count: Arc<AtomicUsize>,
}

impl RequestSpoolAdmission {
    /// Resident bodies are a cache. Reclaim settled objects before denying a
    /// real parsing/forwarding allocation, not only while capturing uploads.
    pub(crate) fn reclaim_for(&self, mut reserve: impl FnMut() -> bool) -> bool {
        loop {
            if reserve() {
                return true;
            }
            if !evict_oldest_settled(&self.usage) {
                return false;
            }
        }
    }

    pub(crate) async fn drain_uploads(&self) {
        self.usage.uploads.wait_for_idle().await;
    }

    pub(crate) fn source(&self, request_id: uuid::Uuid) -> Option<Arc<RequestSpool>> {
        self.usage.sources.lock().ok()?.get(&request_id)?.upgrade()
    }

    pub(crate) fn register_source(&self, request_id: uuid::Uuid, source: &Arc<RequestSpool>) {
        if let Ok(mut sources) = self.usage.sources.lock() {
            sources.insert(request_id, Arc::downgrade(source));
        }
    }

    pub(crate) fn new(_directory: PathBuf) -> Self {
        Self {
            usage: Arc::new(RequestSpoolUsage::default()),
            #[cfg(test)]
            read_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn with_upload_memory(
        mut self,
        memory: Arc<crate::api::proxy::persistence::Persistence>,
    ) -> Self {
        Arc::get_mut(&mut self.usage)
            .expect("upload memory attached before admission sharing")
            .upload_memory = memory;
        self
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
    pub(crate) fn with_memory_store(mut self, store: Arc<MemoryObjectStore>) -> Self {
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
        let durable = self.usage.durable.clone();
        let object_key = {
            request_body_object_key(body_key.unwrap_or(""))
                .or_else(|| request_body_object_key(&uuid::Uuid::now_v7().to_string()))
        };
        // Select the configured backend once, without database or object-store
        // IO. The full locator is carried by the source and its durable receipt;
        // retries and reads must not consult the current write default again.
        let object_key = match (&durable, object_key) {
            (Some(DurableRequestBodies::Archive(archive)), Some(locator)) => {
                let locator = if let Some(key) = request_body_staging_key(&locator) {
                    let key = archive
                        .select_staging_key(key)
                        .map_err(|_| RequestSpoolCaptureError::Unavailable)?;
                    format!("{}/body.json", key.canonical_prefix())
                } else {
                    locator
                };
                Some(locator)
            }
            (_, locator) => locator,
        };
        let metadata_bytes = request_metadata_bytes
            .saturating_add(std::mem::size_of::<RequestSpool>())
            .saturating_add(std::mem::size_of::<Mutex<RequestSpoolLease>>());
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
        let mut length = 0usize;
        let mut hasher = blake3::Hasher::new();
        let mut scanner = super::memory::JsonMemoryScanner::default();
        let mut rejected = None;
        let input = body.into_data_stream().map(|chunk| {
            let chunk = chunk.map_err(|_| RequestSpoolCaptureError::BodyReadRejected)?;
            if chunk.len() > maximum.saturating_sub(length) {
                rejected = Some(RequestSpoolCaptureError::TooLarge);
                return Err(RequestSpoolCaptureError::TooLarge);
            }
            lease
                .lock()
                .map_err(|_| RequestSpoolCaptureError::Unavailable)?
                .record_written(chunk.len());
            length += chunk.len();
            hasher.update(&chunk);
            scanner.observe(&chunk);
            Ok(chunk)
        });
        let (body, carrier) = if let Some(memory) = memory {
            let archive = match &durable {
                Some(DurableRequestBodies::Archive(archive)) => Some(archive.clone()),
                _ => None,
            };
            let staging = object_key
                .as_deref()
                .and_then(request_body_staging_key)
                .unwrap_or_else(|| {
                    use crate::archive_staging::{
                        ArchiveStagingKey, ArchiveStagingOwner, ArchiveStagingPurpose,
                    };
                    let id = uuid::Uuid::now_v7();
                    ArchiveStagingKey::new(
                        ArchiveStagingOwner::ProxyRequest(id),
                        ArchiveStagingPurpose::Request,
                        id,
                    )
                    .expect("server-generated request staging identity")
                });
            let result = BodyCarrier::capture(
                input,
                CarrierCapture::new(memory.shared_budget(), archive, staging),
            )
            .await;
            let carrier = match result {
                Ok(carrier) => carrier,
                Err(_) => {
                    return Err(rejected.unwrap_or(RequestSpoolCaptureError::BodyReadRejected));
                }
            };
            (None, Some(carrier))
        } else {
            // Only callers without a production admission budget use this path.
            // Production bodies always retain allocation leases in BodyCarrier.
            futures_util::pin_mut!(input);
            let mut bytes = Vec::new();
            while let Some(chunk) = input.next().await {
                bytes.extend_from_slice(&chunk?);
            }
            (Some(Bytes::from(bytes)), None)
        };
        let digest = *hasher.finalize().as_bytes();
        let publish = durable.is_some() && object_key.is_some();
        Ok(self
            .finished_spool(CapturedBody {
                body,
                carrier,
                length,
                json_nodes: scanner.observe(&[]),
                digest,
                metadata,
                frames: Vec::new(),
                lease,
                durable,
                object_key,
                durable_settled: !publish,
            })
            .arm())
    }

    fn finished_spool(&self, captured: CapturedBody) -> RequestSpool {
        let carrier_backed = captured.carrier.is_some();
        let durable_ok = captured.durable_settled && captured.durable.is_some();
        RequestSpool {
            shared: Arc::new(SharedBody {
                body: Mutex::new(captured.body),
                carrier: captured.carrier,
                length: captured.length,
                json_nodes: captured.json_nodes,
                digest: captured.digest,
                residency: Arc::new(BodyResidency {
                    frames: Mutex::new(captured.frames),
                    metadata: Mutex::new(captured.metadata),
                    memory_released: AtomicBool::new(false),
                    durable_settled: AtomicBool::new(captured.durable_settled),
                    carrier_backed,
                }),
                lease: captured.lease,
                durable: captured.durable,
                object_key: captured.object_key,
                durable_ok: Arc::new(AtomicBool::new(durable_ok)),
                durable_phase: Arc::new(AtomicU8::new(if durable_ok {
                    DURABLE_COMMITTED
                } else {
                    DURABLE_IN_FLIGHT
                })),
                durable_write: Mutex::new(None),
                archived: AtomicBool::new(false),
                outcome: tokio::sync::watch::channel(if captured.durable_settled {
                    Some(durable_ok)
                } else {
                    None
                })
                .0,
                archive_owner: Mutex::new(None),
                background_upload: !captured.durable_settled,
                archive_ready: tokio::sync::Notify::new(),
                source_dropped: AtomicBool::new(false),
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
    uploads: Arc<crate::proxy_lifecycle::ProxyLifecycleCounter>,
    upload_memory: Arc<crate::api::proxy::persistence::Persistence>,
    used_bytes: AtomicUsize,
    active_files: AtomicUsize,
    durable: Option<DurableRequestBodies>,
    /// Oldest resident first. A settled object copy can replace resident memory.
    residents: Mutex<std::collections::VecDeque<std::sync::Weak<SharedBody>>>,
    sources: Mutex<std::collections::HashMap<uuid::Uuid, std::sync::Weak<RequestSpool>>>,
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
    carrier: Option<Arc<BodyCarrier>>,
    length: usize,
    json_nodes: usize,
    digest: [u8; 32],
    residency: Arc<BodyResidency>,
    lease: Arc<Mutex<RequestSpoolLease>>,
    durable: Option<DurableRequestBodies>,
    object_key: Option<String>,
    durable_ok: Arc<AtomicBool>,
    durable_phase: Arc<AtomicU8>,
    durable_write: Mutex<Option<tokio::task::JoinHandle<()>>>,
    archived: AtomicBool,
    outcome: tokio::sync::watch::Sender<Option<bool>>,
    archive_owner: Mutex<Option<(crate::db::Database, crate::db::ArchiveSpoolIdentity)>>,
    background_upload: bool,
    archive_ready: tokio::sync::Notify,
    source_dropped: AtomicBool,
}

pub(crate) struct RequestSpool {
    shared: Arc<SharedBody>,
    #[cfg(test)]
    read_count: Arc<AtomicUsize>,
}

impl RequestSpool {
    #[cfg(test)]
    pub(crate) async fn abort_writer_for_test(&self) {
        let handle = self.shared.durable_write.lock().unwrap().take().unwrap();
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
    }

    pub(crate) fn object_locator(&self) -> Option<&str> {
        self.shared.object_key.as_deref()
    }

    pub(crate) fn retain_for_archive(
        &self,
        database: crate::db::Database,
        identity: crate::db::ArchiveSpoolIdentity,
    ) {
        self.shared.archived.store(true, Ordering::Release);
        let mut owner = self
            .shared
            .archive_owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *owner = Some((database, identity));
        if !self.shared.background_upload {
            let success = self.shared.outcome.borrow().unwrap_or(false);
            let (database, identity) = owner.take().unwrap();
            let locator = self.shared.object_key.clone().unwrap();
            let length = self.shared.length;
            let digest = self.shared.digest;
            release_resident_memory(&self.shared);
            database.schedule_request_source_completion(identity, locator, length, digest, success);
        }
        self.shared.archive_ready.notify_one();
    }

    pub(crate) async fn upload_outcome(&self) -> bool {
        let mut outcome = self.shared.outcome.subscribe();
        loop {
            if let Some(success) = *outcome.borrow_and_update() {
                return success;
            }
            if outcome.changed().await.is_err() {
                return false;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn shared_bytes(self: &Arc<Self>, bytes: Bytes) -> Bytes {
        struct BodySource {
            bytes: Bytes,
            _source: Arc<RequestSpool>,
        }
        impl AsRef<[u8]> for BodySource {
            fn as_ref(&self) -> &[u8] {
                &self.bytes
            }
        }
        Bytes::from_owner(BodySource {
            bytes,
            _source: self.clone(),
        })
    }
    pub(crate) fn carrier(&self) -> Option<&Arc<BodyCarrier>> {
        self.shared.carrier.as_ref()
    }

    pub(crate) fn len(&self) -> usize {
        self.shared.length
    }

    /// The carrier owns replay independently of the background archive handoff.
    pub(crate) fn release_local_retained(&self) {
        if self.shared.carrier.is_some() {
            release_resident_memory(&self.shared);
        }
    }

    pub(crate) fn json_nodes(&self) -> usize {
        self.shared.json_nodes
    }

    pub(crate) fn reclaim_for(&self, mut reserve: impl FnMut() -> bool) -> bool {
        let usage = self
            .shared
            .lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .usage
            .clone();
        loop {
            if reserve() {
                return true;
            }
            if !evict_oldest_settled(&usage) {
                return false;
            }
        }
    }

    pub(crate) async fn open_stream(
        &self,
    ) -> Result<crate::archive::ArchiveDownload, RequestSpoolReadError> {
        #[cfg(test)]
        self.read_count.fetch_add(1, Ordering::AcqRel);
        let resident = self
            .shared
            .body
            .lock()
            .map_err(|_| RequestSpoolReadError)?
            .clone();
        let download = if let Some(body) = resident {
            crate::archive::ArchiveDownload {
                object_size: self.len() as u64,
                range: 0..self.len() as u64,
                stream: Box::pin(futures_util::stream::once(async move { Ok(body) })),
            }
        } else if let Some(carrier) = &self.shared.carrier {
            crate::archive::ArchiveDownload {
                object_size: carrier.len(),
                range: 0..carrier.len(),
                stream: Box::pin(carrier.open_stream().map(|chunk| {
                    chunk.map_err(|_| std::io::Error::other("request source read failed"))
                })),
            }
        } else {
            if !self.shared.durable_ok.load(Ordering::Acquire) && !self.upload_outcome().await {
                return Err(RequestSpoolReadError);
            }
            let key = self.object_locator().ok_or(RequestSpoolReadError)?;
            match self.shared.durable.as_ref().ok_or(RequestSpoolReadError)? {
                DurableRequestBodies::Archive(archive) => archive
                    .open_stream(key, None)
                    .await
                    .map_err(|_| RequestSpoolReadError)?,
                #[cfg(test)]
                store @ DurableRequestBodies::Memory(_) => {
                    let body = store
                        .get(key, self.len())
                        .await
                        .map_err(|_| RequestSpoolReadError)?;
                    crate::archive::ArchiveDownload {
                        object_size: self.len() as u64,
                        range: 0..self.len() as u64,
                        stream: Box::pin(futures_util::stream::once(async move { Ok(body) })),
                    }
                }
            }
        };
        if download.object_size != self.len() as u64 {
            return Err(RequestSpoolReadError);
        }
        let expected = self.shared.digest;
        let length = self.len();
        let stream = futures_util::stream::unfold(
            (download.stream, blake3::Hasher::new(), 0usize, false),
            move |(mut stream, mut hasher, mut bytes, done)| async move {
                if done {
                    return None;
                }
                let invalid = || std::io::Error::other("request source integrity failed");
                let chunk = match stream.next().await {
                    Some(Ok(chunk)) if chunk.len() <= length.saturating_sub(bytes) => {
                        bytes += chunk.len();
                        hasher.update(&chunk);
                        Ok(chunk)
                    }
                    None if bytes == length && hasher.finalize().as_bytes() == &expected => {
                        return None;
                    }
                    _ => return Some((Err(invalid()), (stream, hasher, bytes, true))),
                };
                Some((chunk, (stream, hasher, bytes, false)))
            },
        );
        Ok(crate::archive::ArchiveDownload {
            object_size: length as u64,
            range: 0..length as u64,
            stream: Box::pin(stream.fuse()),
        })
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
    #[cfg(test)]
    pub(crate) fn release_retained(&self) {
        release_resident_memory(&self.shared);
    }

    pub(crate) fn release_durable_retained(&self) {
        if self.shared.durable_ok.load(Ordering::Acquire) {
            release_resident_memory(&self.shared);
        }
    }

    pub(crate) async fn read_prepaid_projection(
        &self,
        permit: &super::memory::ConversationProjectionPermit,
    ) -> Result<Bytes, RequestSpoolReadError> {
        if !permit.covers(self.len()) {
            return Err(RequestSpoolReadError);
        }
        if let Some(carrier) = self.carrier() {
            let mut bytes = vec![0; self.len()];
            carrier
                .read_into(0, &mut bytes)
                .await
                .map_err(|_| RequestSpoolReadError)?;
            return verified_body(&self.shared, Bytes::from(bytes));
        }
        self.read_legacy_body().await
    }

    /// Test-only materialization: production callers use the prepaid projection
    /// reader or stream. The collection belongs to the test's expected-value
    /// workspace, not an unaccounted production body allocation.
    #[cfg(test)]
    pub(crate) async fn read_all(&self) -> Result<Bytes, RequestSpoolReadError> {
        if self.carrier().is_none() {
            return self.read_legacy_body().await;
        }
        self.read_count.fetch_add(1, Ordering::AcqRel);
        let mut source = self.open_stream().await?;
        let mut body = Vec::new();
        while let Some(chunk) = source.stream.next().await {
            body.extend_from_slice(&chunk.map_err(|_| RequestSpoolReadError)?);
        }
        verified_body(&self.shared, Bytes::from(body))
    }

    async fn read_legacy_body(&self) -> Result<Bytes, RequestSpoolReadError> {
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
        if self.shared.carrier.is_some() {
            return Err(RequestSpoolReadError);
        }
        if !self.shared.durable_ok.load(Ordering::Acquire) && !self.upload_outcome().await {
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
        self.shared.source_dropped.store(true, Ordering::Release);
        self.shared.archive_ready.notify_one();
        if let Some(request_id) = self.object_locator().and_then(source_request_id)
            && let Ok(lease) = self.shared.lease.lock()
            && let Ok(mut sources) = lease.usage.sources.lock()
            && sources
                .get(&request_id)
                .is_some_and(|source| source.strong_count() == 0)
        {
            sources.remove(&request_id);
        }
        if self.shared.archived.load(Ordering::Acquire) {
            return;
        }
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
    for _ in 0..residents.len() {
        let Some(resident) = residents.pop_front() else {
            break;
        };
        let Some(shared) = resident.upgrade() else {
            continue;
        };
        let reclaimable = shared
            .body
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(Bytes::is_unique);
        match reclaimable {
            None => continue,
            Some(false) => {
                residents.push_back(Arc::downgrade(&shared));
                continue;
            }
            Some(true) => {}
        }
        if shared.carrier.is_none()
            && (!shared.residency.durable_settled.load(Ordering::Acquire)
                || !shared.durable_ok.load(Ordering::Acquire))
        {
            residents.push_back(Arc::downgrade(&shared));
            continue;
        }
        release_resident_memory(&shared);
        return true;
    }
    false
}

fn publish_durable(shared: &Arc<SharedBody>) {
    let Some(store) = shared.durable.clone() else {
        return;
    };
    let Some(key) = shared.object_key.clone() else {
        return;
    };
    let bytes = shared
        .body
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .filter(|_| shared.carrier.is_none())
        .cloned();
    let carrier = shared.carrier.clone();
    if bytes.is_none() && carrier.is_none() {
        return;
    }
    let ok = Arc::clone(&shared.durable_ok);
    let phase = Arc::clone(&shared.durable_phase);
    let residency = shared.residency.clone();
    let outcome = shared.outcome.clone();
    let source = shared.clone();
    let upload_memory = shared
        .lease
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .usage
        .upload_memory
        .clone();
    let activity = shared
        .lease
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .usage
        .uploads
        .start();
    let handle = tokio::spawn(async move {
        let _activity = activity;
        let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
        let upload = async {
            let retry_upload = async {
                loop {
                    let result = match (&bytes, &carrier) {
                        (Some(bytes), _) => store.put(&key, bytes.clone()).await,
                        (_, Some(carrier)) => {
                            store.put_carrier(&key, carrier, &upload_memory).await
                        }
                        _ => Err(()),
                    };
                    if result.is_ok() {
                        break true;
                    }
                    if source.source_dropped.load(Ordering::Acquire)
                        && !source.archived.load(Ordering::Acquire)
                    {
                        break false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            };
            let wrote = tokio::select! {
                biased;
                _ = cancelled => false,
                result = retry_upload => result,
            };
            drop(bytes);
            if wrote {
                match phase.compare_exchange(
                    DURABLE_IN_FLIGHT,
                    DURABLE_COMMITTED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        ok.store(true, Ordering::Release);
                    }
                    Err(DURABLE_DROPPED) => {
                        store.delete(&key).await;
                    }
                    Err(_) => {}
                }
                if source.archived.load(Ordering::Acquire) {
                    release_resident_memory(&source);
                }
                residency.durable_settled.store(true, Ordering::Release);
                residency.release_if_idle();
            } else {
                tracing::warn!("request body stayed in memory; object storage write failed");
            }
            outcome.send_replace(Some(wrote));
        };
        let completion = async {
            loop {
                let ready = source.archive_ready.notified();
                let owner = source
                    .archive_owner
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some((database, identity)) = owner {
                    let lease = async {
                        let wrote = database
                            .wait_request_source_upload(
                                identity,
                                &key,
                                source.length,
                                source.digest,
                                outcome.subscribe(),
                            )
                            .await;
                        if wrote.is_none() {
                            let _ = cancel.send(());
                        }
                        wrote
                    };
                    let handoff = async {
                        let mut settled = outcome.subscribe();
                        let success = loop {
                            if let Some(success) = *settled.borrow_and_update() {
                                break success;
                            }
                            if settled.changed().await.is_err() {
                                break false;
                            }
                        };
                        let settlement = if success {
                            let (object, settlement) = database
                                .publish_request_source(
                                    identity,
                                    &key,
                                    source.length,
                                    source.digest,
                                )
                                .await;
                            // Either the immutable raw root or verified semantic
                            // retirement durably owns this exact object.
                            if let Some(carrier) = &source.carrier
                                && let Ok(archive) = database.body_archive()
                                && carrier
                                    .handoff_verified_publication(archive.clone(), object)
                                    .is_err()
                            {
                                tracing::error!(
                                    "request source durable handoff identity mismatch; retaining RAM"
                                );
                            }
                            Some(settlement)
                        } else {
                            None
                        };
                        release_resident_memory(&source);
                        residency.durable_settled.store(true, Ordering::Release);
                        residency.release_if_idle();
                        let permit = if matches!(
                            settlement.as_ref(),
                            Some(crate::archive::RawPublicationSettlement::Retired { .. })
                        ) {
                            Some(database.request_source_completion().await)
                        } else if success {
                            database.try_request_source_completion()
                        } else {
                            Some(database.request_source_completion().await)
                        };
                        (permit, settlement)
                    };
                    let (wrote, (permit, settlement)) = tokio::join!(lease, handoff);
                    if let (Some(_permit), Some(wrote)) = (permit, wrote) {
                        if wrote {
                            database
                                .complete_uploaded_request_source(
                                    identity,
                                    &key,
                                    source.length,
                                    source.digest,
                                    settlement.expect("uploaded source has a publication outcome"),
                                )
                                .await;
                        } else {
                            database
                                .complete_request_source(
                                    identity,
                                    &key,
                                    source.length,
                                    source.digest,
                                    false,
                                )
                                .await;
                        }
                    }
                    break;
                }
                if source.source_dropped.load(Ordering::Acquire) {
                    let mut settled = outcome.subscribe();
                    while settled.borrow_and_update().is_none() {
                        if settled.changed().await.is_err() {
                            break;
                        }
                    }
                    residency.durable_settled.store(true, Ordering::Release);
                    residency.release_if_idle();
                    break;
                }
                ready.await;
            }
        };
        tokio::join!(upload, completion);
    });
    *shared
        .durable_write
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(handle);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestSpoolCaptureError {
    TooLarge,
    BodyReadRejected,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestSpoolReadError;

impl std::fmt::Display for RequestSpoolCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request source capture failed")
    }
}

#[cfg(test)]
mod archive_source_tests;

#[cfg(test)]
mod shutdown_tests;

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, path::Path, sync::Arc};

    use futures_util::stream;

    use super::*;

    fn file_count(directory: &Path) -> usize {
        std::fs::read_dir(directory).map_or(0, |entries| entries.count())
    }

    #[tokio::test]
    async fn pending_object_upload_keeps_the_ram_source_charged() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let resume = store.pause_next_write();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(1024 * 1024);
        let body = Bytes::from(vec![b'x'; 128 * 1024]);
        let source = admission
            .capture_with_memory(
                Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                Some(&budget.reservation()),
                0,
                None,
            )
            .await
            .unwrap();
        while store.entered_writes.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        assert!(budget.snapshot().0 > 0);
        assert!(!source.shared.durable_ok.load(Ordering::Acquire));
        source.release_local_retained();
        assert!(
            budget.snapshot().0 >= body.len(),
            "live RAM source retains its real allocation lease"
        );
        assert_eq!(source.read_all().await.unwrap(), body);
        assert_eq!(store.reads.load(Ordering::Acquire), 0);
        assert!(!source.shared.durable_ok.load(Ordering::Acquire));
        resume.send(()).unwrap();
        assert!(source.upload_outcome().await);
        assert_eq!(source.read_all().await.unwrap(), body);
    }

    #[tokio::test]
    async fn memory_capture_round_trips_and_drop_releases_ownership() {
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
    async fn ram_capture_creates_no_restart_orphan() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("request-spool");
        let admission = RequestSpoolAdmission::new(path.clone());
        let spool = admission
            .capture(Body::from("sensitive"), 1024, None)
            .await
            .unwrap();
        assert_eq!(
            file_count(&path),
            0,
            "active capture must not create a body file"
        );
        drop(spool);
        drop(admission);
        let restarted = RequestSpoolAdmission::new(path.clone());
        assert_eq!(restarted.snapshot().used_bytes, 0);
        assert_eq!(file_count(&path), 0);
    }

    #[tokio::test]
    async fn available_ram_forwards_without_archive_or_local_files() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned());
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(2 * 1024 * 1024);
        let memory = budget.reservation();
        let body = Bytes::from(vec![b'x'; 512 * 1024]);
        let spool = admission
            .capture_with_memory(
                Body::from(body.clone()),
                usize::MAX,
                Some(body.len()),
                Some(&memory),
                0,
                Some("11111111-1111-4111-8111-111111111111"),
            )
            .await
            .unwrap();
        assert_eq!(spool.read_all().await.unwrap(), body);
        assert!(spool.object_locator().is_some());
        assert!(
            !spool.upload_outcome().await,
            "RAM transport is not a durable archive receipt"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            file_count(directory.path()),
            0,
            "capture never creates an application body file"
        );
        drop(spool);
        assert_eq!(admission.snapshot().active_files, 0);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[tokio::test]
    async fn pressure_spills_to_typed_objects_without_touching_the_body_directory() {
        let directory = tempfile::tempdir().unwrap();
        let archive = crate::archive::ArchiveStore::memory_for_test();
        let admission = RequestSpoolAdmission::new(directory.path().join("must-not-exist"))
            .with_archive(archive);
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(128 * 1024);
        let expected = Bytes::from(vec![b'p'; 512 * 1024]);
        let source = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            admission.capture_with_memory(
                Body::from(expected.clone()),
                usize::MAX,
                Some(expected.len()),
                Some(&budget.reservation()),
                0,
                Some("11111111-1111-4111-8111-111111111111"),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!directory.path().join("must-not-exist").exists());
        assert!(source.shared.carrier.is_some());
        let mut stream = source.open_stream().await.unwrap().stream;
        let mut hash = blake3::Hasher::new();
        let mut count = 0usize;
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.unwrap();
            hash.update(&bytes);
            count += bytes.len();
        }
        assert_eq!(count, expected.len());
        assert_eq!(hash.finalize(), blake3::hash(&expected));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), source.upload_outcome())
                .await
                .unwrap()
        );
        assert_eq!(file_count(directory.path()), 0);
    }

    #[tokio::test]
    async fn prepaid_projection_reads_without_acquiring_a_second_body_lease() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned());
        let budget = super::super::memory::ProxyMemoryBudget::new(192 * 1024);
        let memory = budget.reservation();
        let bytes = Bytes::from(vec![b'x'; 64 * 1024]);
        let source = admission
            .capture_with_memory(
                Body::from(bytes.clone()),
                usize::MAX,
                Some(bytes.len()),
                Some(&memory),
                0,
                None,
            )
            .await
            .unwrap();
        let projection = memory
            .reserve_conversation_projection(bytes.len(), 1, tokio::time::Instant::now())
            .unwrap();
        let before = budget.snapshot().0;
        assert_eq!(before, 192 * 1024);
        let actual = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            source.read_prepaid_projection(&projection),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(actual, bytes);
        assert_eq!(budget.snapshot().0, before);
        drop(actual);
        drop(projection);
        drop(source);
        assert_eq!(budget.snapshot().0, 0);
        assert_eq!(file_count(directory.path()), 0);
    }

    #[tokio::test]
    async fn failed_archive_upload_does_not_block_budgeted_ram_replay() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        store.fail_writes.store(true, Ordering::Release);
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(2 * 1024 * 1024);
        let memory = budget.reservation();
        let body = Bytes::from(vec![b'z'; 512 * 1024]);
        let spool = admission
            .capture_with_memory(
                Body::from(body.clone()),
                usize::MAX,
                Some(body.len()),
                Some(&memory),
                0,
                Some("22222222-2222-4222-8222-222222222222"),
            )
            .await
            .unwrap();
        assert_eq!(spool.read_all().await.unwrap(), body);
        assert!(!spool.shared.durable_ok.load(Ordering::Acquire));
        store.fail_writes.store(false, Ordering::Release);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), spool.upload_outcome())
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .objects
                .lock()
                .unwrap()
                .get(spool.object_locator().unwrap()),
            Some(&body)
        );
        assert_eq!(
            spool.read_all().await.unwrap(),
            body,
            "archive and forwarding readers have independent offsets"
        );
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
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(512 * 1024);
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
        assert_eq!(
            store.reads.load(Ordering::Acquire),
            0,
            "both sources retain independent budgeted RAM readers"
        );
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
    async fn carrier_backed_unsettled_body_is_reclaimed_for_the_new_upload() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let release = store.pause_next_write();
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(512 * 1024);
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
        assert_eq!(
            reads_after_spill, 0,
            "RAM replay must not wait for the pending archive upload"
        );
        assert!(oldest.shared.body.lock().unwrap().is_none());
        assert!(newest.shared.body.lock().unwrap().is_none());
        assert!(newest.shared.carrier.is_some());
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
