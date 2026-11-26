use bytes::Bytes;
use getrandom::fill;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(test)]
use tracing::Instrument;

use crate::{AppState, db::ArchiveSpoolIdentity, error::AppError};

// Deliberately no Debug: prepared ciphertext must not enter logs.
pub(crate) struct PreparedArchiveBatch {
    identity: ArchiveSpoolIdentity,
    purpose: super::BufferedArchivePurpose,
    compression_enabled: bool,
    chunks: Vec<crate::db::ArchiveSpoolChunk>,
}

impl PreparedArchiveBatch {
    pub(crate) fn into_chunks_for(
        self,
        archive: &BufferedArchive<'_>,
    ) -> Option<Vec<crate::db::ArchiveSpoolChunk>> {
        (self.identity == archive.identity
            && self.purpose == archive.purpose
            && self.compression_enabled == archive.compression_enabled)
            .then_some(self.chunks)
    }
}

/// A replayable buffered capture. Only the response bytes and 12-byte random
/// nonces are retained; ciphertext is generated in bounded database batches.
pub(crate) struct BufferedArchive<'a> {
    identity: ArchiveSpoolIdentity,
    purpose: super::BufferedArchivePurpose,
    body: &'a Bytes,
    pepper: &'a [u8],
    compression_enabled: bool,
    nonces: Vec<[u8; 12]>,
}

impl<'a> BufferedArchive<'a> {
    pub(crate) fn new(
        identity: ArchiveSpoolIdentity,
        purpose: super::BufferedArchivePurpose,
        body: &'a Bytes,
        pepper: &'a [u8],
        compression_enabled: bool,
    ) -> Result<Self, AppError> {
        // Raw request JSON and raw response JSON are split into CHUNK_BYTES
        // pieces and archived in full. The former 64MiB plain ceiling is not
        // a capture cutoff.
        let chunk_count = body.len().div_ceil(super::CHUNK_BYTES);
        let mut nonces = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            let mut nonce = [0_u8; 12];
            fill(&mut nonce).map_err(|_| AppError::internal("producer_new_1"))?;
            nonces.push(nonce);
        }
        Ok(Self {
            identity,
            purpose,
            body,
            pepper,
            compression_enabled,
            nonces,
        })
    }

    pub(crate) fn body(&self) -> &'a Bytes {
        self.body
    }

    pub(crate) fn identity(&self) -> ArchiveSpoolIdentity {
        self.identity
    }

    pub(crate) fn purpose(&self) -> super::BufferedArchivePurpose {
        self.purpose
    }

    pub(crate) fn sealed_len(&self, byte_count: usize) -> Option<usize> {
        super::cipher::sealed_len(byte_count)
    }

    pub(crate) fn seal(&self, seq: usize) -> Result<String, AppError> {
        let nonce = self
            .nonces
            .get(seq)
            .copied()
            .ok_or(AppError::internal("producer_seal_1"))?;
        let start = seq
            .checked_mul(super::CHUNK_BYTES)
            .ok_or(AppError::internal("producer_seal_2"))?;
        let end = (start + super::CHUNK_BYTES).min(self.body.len());
        let bytes = self
            .body
            .get(start..end)
            .ok_or(AppError::internal("producer_seal_3"))?;
        let sealed = super::cipher::seal_for_purpose_with_nonce_and_compression(
            self.identity,
            i64::try_from(seq).map_err(|_| AppError::internal("producer_seal_1"))?,
            bytes,
            self.pepper,
            self.purpose,
            nonce,
            self.compression_enabled,
        )?;
        debug_assert!(
            self.sealed_len(bytes.len())
                .is_some_and(|upper| sealed.len() <= upper)
        );
        Ok(sealed)
    }

    /// Prepare only the existing bounded first database batch before any
    /// archive/account transaction is opened. This keeps peak ciphertext
    /// memory unchanged while removing encryption and bind construction from
    /// the global budget lock for the common one-batch request.
    pub(crate) async fn prepare_first_batch(&self) -> Result<PreparedArchiveBatch, AppError> {
        let chunks = (0..self.nonces.len().min(super::CAPTURE_INSERT_BATCH_CHUNKS))
            .map(|seq| {
                let start = seq
                    .checked_mul(super::CHUNK_BYTES)
                    .ok_or(AppError::internal("producer_prepare_first_batch_1"))?;
                let end = (start + super::CHUNK_BYTES).min(self.body.len());
                Ok(crate::db::ArchiveSpoolChunk {
                    seq: i64::try_from(seq)
                        .map_err(|_| AppError::internal("producer_prepare_first_batch_1"))?,
                    ciphertext: self.seal(seq)?,
                    byte_count: i64::try_from(end - start)
                        .map_err(|_| AppError::internal("producer_prepare_first_batch_2"))?,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        #[cfg(test)]
        pause_request_preseal_for_test(self.identity.request_id).await;
        Ok(PreparedArchiveBatch {
            identity: self.identity,
            purpose: self.purpose,
            compression_enabled: self.compression_enabled,
            chunks,
        })
    }
}

#[cfg(test)]
type RequestPresealPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
    uuid::Uuid,
);

#[cfg(test)]
static PAUSE_REQUEST_PRESEAL: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<uuid::Uuid, RequestPresealPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) fn pause_next_request_preseal_for_test(
    request_id: uuid::Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entering, release, _) = register_request_preseal_pause_for_test(request_id);
    (entering, release)
}

#[cfg(test)]
pub(crate) struct RequestPresealRegistration {
    request_id: uuid::Uuid,
    registration_id: uuid::Uuid,
}

#[cfg(test)]
impl Drop for RequestPresealRegistration {
    fn drop(&mut self) {
        let mut pauses = PAUSE_REQUEST_PRESEAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pauses
            .get(&self.request_id)
            .is_some_and(|(_, _, registration_id)| *registration_id == self.registration_id)
        {
            pauses.remove(&self.request_id);
        }
    }
}

#[cfg(test)]
pub(crate) fn scoped_request_preseal_pause_for_test(
    request_id: uuid::Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    RequestPresealRegistration,
) {
    let (entering, release, registration_id) = register_request_preseal_pause_for_test(request_id);
    (
        entering,
        release,
        RequestPresealRegistration {
            request_id,
            registration_id,
        },
    )
}

#[cfg(test)]
fn register_request_preseal_pause_for_test(
    request_id: uuid::Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    uuid::Uuid,
) {
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let registration_id = uuid::Uuid::new_v4();
    let mut pauses = PAUSE_REQUEST_PRESEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!pauses.contains_key(&request_id));
    pauses.insert(request_id, (entered, released, registration_id));
    (entering, release, registration_id)
}

#[cfg(test)]
async fn pause_request_preseal_for_test(request_id: uuid::Uuid) {
    let pause = PAUSE_REQUEST_PRESEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&request_id);
    if let Some((entered, released, _)) = pause {
        let _ = entered.send(());
        let _ = released.await;
    }
}

#[cfg(test)]
pub(crate) fn encrypt_buffered(
    identity: ArchiveSpoolIdentity,
    purpose: super::BufferedArchivePurpose,
    body: &Bytes,
    pepper: &[u8],
) -> Result<Vec<crate::db::ArchiveSpoolChunk>, AppError> {
    // Same full-body chunking as BufferedArchive::new. The former 64MiB plain
    // ceiling is not a capture cutoff.
    body.chunks(super::CHUNK_BYTES)
        .enumerate()
        .map(|(seq, bytes)| {
            Ok(crate::db::ArchiveSpoolChunk {
                seq: seq as i64,
                byte_count: bytes.len() as i64,
                ciphertext: super::cipher::seal_for_purpose(
                    identity, seq as i64, bytes, pepper, purpose,
                )?,
            })
        })
        .collect()
}

/// Compatibility capture entry point. Production buffered admission/settlement
/// use the transaction-composing APIs; no buffered SQL is deadline-cancelled.
#[cfg(test)]
pub(crate) async fn capture_buffered(
    state: &AppState,
    identity: ArchiveSpoolIdentity,
    purpose: super::BufferedArchivePurpose,
    body: Bytes,
) -> bool {
    let started = tokio::time::Instant::now();
    let chunks =
        match encrypt_buffered(identity, purpose, &body, state.config.key_pepper.as_bytes()) {
            Ok(chunks) => chunks,
            Err(_) => {
                tracing::warn!(
                    phase = "encrypt",
                    error_code = "capture_failed",
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "buffered archive gap"
                );
                return false;
            }
        };
    match state
        .db
        .capture_buffered_archive_spool(identity, purpose, &chunks)
        .await
    {
        Ok(true) => true,
        result => {
            let error_code = if matches!(result, Ok(false)) {
                "capacity"
            } else {
                "capture_failed"
            };
            tracing::warn!(
                phase = "database_ack",
                error_code,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "buffered archive gap"
            );
            false
        }
    }
}

#[cfg(test)]
static FAIL_NEXT_APPEND: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
type BeginAckPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
static PAUSE_NEXT_BEGIN_ACK: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, BeginAckPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static PAUSE_NEXT_BEGIN: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, BeginAckPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) fn pause_next_begin_for_test(
    state: &AppState,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let mut pauses = PAUSE_NEXT_BEGIN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        pauses
            .insert(state.config.database_url.clone(), (entered, released))
            .is_none()
    );
    (entering, release)
}

#[cfg(test)]
async fn pause_begin_for_test(state: &AppState) {
    let pause = PAUSE_NEXT_BEGIN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&state.config.database_url);
    if let Some((entered, released)) = pause {
        let _ = entered.send(());
        let _ = released.await;
    }
}

#[cfg(test)]
pub(crate) fn pause_next_begin_ack_for_test(
    state: &AppState,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let mut pauses = PAUSE_NEXT_BEGIN_ACK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        pauses
            .insert(state.config.database_url.clone(), (entered, released))
            .is_none()
    );
    (entering, release)
}

#[cfg(test)]
async fn pause_begin_ack_for_test(state: &AppState) {
    let pause = PAUSE_NEXT_BEGIN_ACK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&state.config.database_url);
    if let Some((entered, released)) = pause {
        let _ = entered.send(());
        let _ = released.await;
    }
}

#[cfg(test)]
/// Latch one producer failure without opening a database transaction, so tests
/// can isolate capture failure from persistence failure on the following gap.
pub(crate) fn fail_next_append_for_test(state: &AppState) {
    let mut latched = FAIL_NEXT_APPEND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(latched.insert(state.config.database_url.clone()));
}

#[cfg(test)]
fn take_append_failure_for_test(state: &AppState) -> bool {
    FAIL_NEXT_APPEND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&state.config.database_url)
}

pub(crate) struct ResponseArchiveProducer {
    identity: ArchiveSpoolIdentity,
    #[cfg(test)]
    state: AppState,
    capture: CarrierCaptureInput,
    writer: super::OwnedTask<()>,
    active: Arc<AtomicBool>,
    _queue_memory: Arc<CaptureQueueMemory>,
}

#[cfg(test)]
struct QueuedArchiveChunk {
    bytes: Bytes,
    _memory: Option<tokio::sync::OwnedSemaphorePermit>,
}

pub(crate) struct ResponseArchiveSettlement {
    _writer: super::OwnedTask<()>,
}

impl ResponseArchiveSettlement {
    #[cfg(test)]
    pub(crate) async fn wait(mut self) {
        if let Some(Err(error)) = self._writer.wait().await {
            tracing::warn!(
                stage = "response_spool_writer",
                error_category = error.diagnostic_category(),
                "response archive writer failed after terminal handoff"
            );
        }
    }

    #[cfg(test)]
    pub(super) async fn wait_result_for_test(mut self) -> Option<Result<(), AppError>> {
        self._writer.wait().await
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(released: tokio::sync::oneshot::Receiver<()>) -> Self {
        Self {
            _writer: super::OwnedTask::spawn(
                async move {
                    released
                        .await
                        .map_err(|_| AppError::internal("producer_pending_for_test_1"))?;
                    Ok(())
                },
                "response_spool_writer_test",
                None,
            ),
        }
    }
}

#[cfg(test)]
pub(super) struct ResponseArchiveWriter {
    state: AppState,
    identity: ArchiveSpoolIdentity,
    compression_enabled: bool,
    seq: i64,
    bytes: i64,
    append_attempts: u64,
    append_wait: std::time::Duration,
    max_append_wait: std::time::Duration,
}

#[cfg(test)]
impl Drop for ResponseArchiveWriter {
    fn drop(&mut self) {
        tracing::info!(request_id = %self.identity.request_id, phase = "response_spool_writer_summary",
            append_attempts = self.append_attempts, acknowledged_chunks = self.seq, bytes = self.bytes,
            append_wait_ms = self.append_wait.as_millis() as u64,
            max_append_wait_ms = self.max_append_wait.as_millis() as u64,
            "response archive writer append summary");
    }
}

struct CapturedResponse {
    source: Arc<crate::gateway_body::carrier::BodyCarrier>,
    staging: crate::archive_staging::ArchiveStagingKey,
}

struct CaptureChunk {
    bytes: Bytes,
    credit: Option<tokio::sync::OwnedSemaphorePermit>,
    acknowledged: tokio::sync::oneshot::Sender<()>,
    #[cfg(test)]
    pause: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
}

struct CaptureReceipt {
    acknowledged: tokio::sync::oneshot::Sender<()>,
    #[cfg(test)]
    pause: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
}

struct CarrierCaptureInput {
    budget: Arc<crate::gateway_body::memory::ProxyMemoryBudget>,
    source: Option<tokio::sync::mpsc::UnboundedSender<CaptureChunk>>,
    progress: tokio::sync::watch::Sender<(u64, bool)>,
    length: u64,
    #[cfg(test)]
    write_pause: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    #[cfg(test)]
    owner: std::sync::Weak<()>,
}

impl CarrierCaptureInput {
    async fn append(&mut self, bytes: Bytes) -> std::io::Result<()> {
        for offset in (0..bytes.len()).step_by(super::CHUNK_BYTES) {
            let end = (offset + super::CHUNK_BYTES).min(bytes.len());
            let (acknowledged, completion) = tokio::sync::oneshot::channel();
            self.source
                .as_ref()
                .ok_or_else(|| std::io::Error::other("capture closed"))?
                .send(CaptureChunk {
                    bytes: bytes.slice(offset..end),
                    credit: None,
                    acknowledged,
                    #[cfg(test)]
                    pause: self.write_pause.take(),
                })
                .map_err(|_| std::io::Error::other("capture closed"))?;
            // The receiver acknowledges only when capture has taken ownership
            // of the previous chunk, rather than merely queueing its bytes.
            completion
                .await
                .map_err(|_| std::io::Error::other("capture closed"))?;
            self.length += (end - offset) as u64;
            self.progress.send_replace((self.length, false));
        }
        Ok(())
    }
}

fn carrier_capture_stream(
    receiver: tokio::sync::mpsc::UnboundedReceiver<CaptureChunk>,
) -> impl futures_util::Stream<
    Item = Result<(Bytes, Option<tokio::sync::OwnedSemaphorePermit>), AppError>,
> + Send {
    futures_util::stream::unfold(
        (receiver, None::<CaptureReceipt>),
        |(mut receiver, previous)| async move {
            if let Some(previous) = previous {
                #[cfg(test)]
                if let Some((entered, release)) = previous.pause {
                    let _ = entered.send(());
                    let _ = release.await;
                }
                let _ = previous.acknowledged.send(());
            }
            let chunk = receiver.recv().await?;
            Some((
                Ok((chunk.bytes, chunk.credit)),
                (
                    receiver,
                    Some(CaptureReceipt {
                        acknowledged: chunk.acknowledged,
                        #[cfg(test)]
                        pause: chunk.pause,
                    }),
                ),
            ))
        },
    )
}

struct CaptureQueueMemory {
    // Legacy test admission and the background multipart envelope retain their
    // own permits. Production source allocations belong to BodyCarrier.
    _local_metrics: Option<crate::metrics::MemoryUsageGuard>,
    _archive_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    reservation: Option<Arc<crate::gateway_body::memory::ProxyMemoryReservation>>,
    workspace: Option<Arc<crate::api::proxy::persistence::WorkspaceGuard>>,
}

impl Drop for CaptureQueueMemory {
    fn drop(&mut self) {
        if let Some(reservation) = &self.reservation {
            reservation.release(
                super::CAPTURE_MEMORY_BYTES,
                crate::gateway_body::memory::CAPTURE_MEMORY_WEIGHT,
            );
        }
    }
}

pub(super) async fn fence_failed_capture(
    state: &AppState,
    identity: ArchiveSpoolIdentity,
    target: CaptureTarget,
) -> Result<(), AppError> {
    #[cfg(test)]
    super::fence_probe::observe(state).await;
    let result = target.publish_failure(&state.archive, identity).await;
    if let Err(error) = &result {
        // Keep only the failure fact when object storage cannot acknowledge it.
        // The existing background owner retries it; incomplete bytes never seal.
        crate::api::proxy::persistence::record_capture_failure_target(state, identity, target);
        tracing::warn!(
            request_id = %identity.request_id,
            stage = "response_spool_failed_ack_fence",
            error_category = error.diagnostic_category(),
            "failed response archive acknowledgement could not be fenced"
        );
    }
    result
}

struct OriginalAdmission {
    state: Option<AppState>,
    identity: ArchiveSpoolIdentity,
    target: CaptureTarget,
}

#[derive(Clone, Copy)]
pub(crate) enum CaptureTarget {
    Response,
    DiscardedAttempt(crate::archive::RawResponseAttempt),
}

impl CaptureTarget {
    pub(crate) async fn publish_failure(
        self,
        archive: &crate::archive::ArchiveStore,
        identity: ArchiveSpoolIdentity,
    ) -> Result<(), AppError> {
        match self {
            Self::Response => {
                archive
                    .publish_raw_capture_failure(
                        identity.tenant_id,
                        identity.request_id,
                        "response",
                    )
                    .await
            }
            Self::DiscardedAttempt(attempt) => {
                archive
                    .publish_response_attempt_failure(
                        identity.tenant_id,
                        identity.request_id,
                        attempt,
                    )
                    .await
            }
        }
    }
    async fn publish(
        self,
        archive: &crate::archive::ArchiveStore,
        identity: ArchiveSpoolIdentity,
        object: &crate::archive::CasArchiveObject,
    ) -> Result<crate::archive::RawPublicationSettlement, AppError> {
        match self {
            Self::Response => {
                archive
                    .settle_raw_capture(identity.tenant_id, identity.request_id, "response", object)
                    .await
            }
            Self::DiscardedAttempt(attempt) => archive
                .publish_response_attempt(identity.tenant_id, identity.request_id, attempt, object)
                .await
                .map(|()| crate::archive::RawPublicationSettlement::Published),
        }
    }
}

impl OriginalAdmission {
    fn disarm(mut self) {
        self.state.take();
    }
}

impl Drop for OriginalAdmission {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            crate::api::proxy::persistence::record_capture_failure_target(
                &state,
                self.identity,
                self.target,
            );
        }
    }
}

impl ResponseArchiveProducer {
    #[cfg(test)]
    pub(crate) async fn begin_original(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
    ) -> Option<Self> {
        Self::admit_original(state, identity).await
    }

    #[cfg(test)]
    pub(crate) fn admit_original(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
    ) -> impl std::future::Future<Output = Option<Self>> + Send + 'static {
        Self::admit_target(state, identity, CaptureTarget::Response)
    }

    pub(crate) fn admit_target(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
        target: CaptureTarget,
    ) -> impl std::future::Future<Output = Option<Self>> + Send + 'static {
        Self::admit_target_with_scope(state, identity, target, None)
    }

    pub(crate) fn admit_target_with_scope(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
        target: CaptureTarget,
        scope: Option<crate::archive::TrustedArchiveWriteScope>,
    ) -> impl std::future::Future<Output = Option<Self>> + Send + 'static {
        let scope = scope.filter(|scope| scope.belongs_to(identity.tenant_id));
        let state = state.clone();
        let admission = OriginalAdmission {
            state: Some(state.clone()),
            identity,
            target,
        };
        async move {
            // Capture takes real RAM/object ownership independently of upload
            // admission. The writer retains it until the CAS/root acknowledgement.
            let producer = Self::begin_admitted(&state, identity, None, None, target, scope);
            admission.disarm();
            Some(producer)
        }
    }

    #[cfg(test)]
    pub(crate) fn begin(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
        memory: Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
    ) -> Option<Self> {
        let archive_permit = match state
            .proxy_archive_stream_permits
            .clone()
            .try_acquire_owned()
        {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!(request_id = %identity.request_id, stage = "response_spool_admission", outcome = "capacity", "proxy archive gap");
                return None;
            }
        };
        if !memory.try_grow(
            super::CAPTURE_MEMORY_BYTES,
            crate::gateway_body::memory::CAPTURE_MEMORY_WEIGHT,
        ) {
            tracing::warn!(request_id = %identity.request_id, stage = "response_spool_memory_admission", "proxy archive gap");
            return None;
        }
        Some(Self::begin_admitted(
            state,
            identity,
            Some(memory),
            Some(archive_permit),
            CaptureTarget::Response,
            None,
        ))
    }

    fn begin_admitted(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
        memory: Option<Arc<crate::gateway_body::memory::ProxyMemoryReservation>>,
        archive_permit: Option<tokio::sync::OwnedSemaphorePermit>,
        target: CaptureTarget,
        scope: Option<crate::archive::TrustedArchiveWriteScope>,
    ) -> Self {
        let queue_memory = Arc::new(CaptureQueueMemory {
            _local_metrics: None,
            reservation: memory,
            workspace: None,
            _archive_permit: archive_permit,
        });
        let (source, source_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (progress, progress_receiver) = tokio::sync::watch::channel((0_u64, false));
        let active = Arc::new(AtomicBool::new(true));
        let writer_active = active.clone();
        let failure_active = active.clone();
        let capture_owner = Arc::new(());
        let source_owner = capture_owner.clone();
        let capture_state = state.clone();
        let capture_progress = progress_receiver.clone();
        let capture_active = active.clone();
        let captured = super::OwnedTask::spawn(
            async move {
                let _source_owner = source_owner;
                let key = capture_state
                    .archive
                    .new_response_staging_key(identity.request_id, scope)?;
                let capture = crate::gateway_body::carrier::CarrierCapture::new(
                    capture_state
                        .persistence
                        .stream_memory
                        .reservation()
                        .shared_budget(),
                    Some(capture_state.archive.clone()),
                    key,
                )
                .with_upload_workspace(capture_state.persistence.clone());
                tokio::select! {
                    result = crate::gateway_body::carrier::BodyCarrier::capture_owned(carrier_capture_stream(source_receiver), capture) => result.map(|source| CapturedResponse { source, staging: key }),
                    () = capture_abandoned(capture_progress, capture_active) => Err(AppError::internal("response_capture_cancelled")),
                }
            },
            "response_carrier_capture",
            None,
        );
        #[cfg(test)]
        let producer_owner = capture_owner.clone();
        let mut writer_state = state.clone();
        writer_state.db = state.persistence_db.clone();
        writer_state.db.refresh_closed_persistence_pool();
        let writer_memory = queue_memory.clone();
        let writer = super::OwnedTask::spawn(
            async move {
                let _queue_memory = writer_memory.clone();
                let _source_owner = capture_owner;
                match run_response_archive_writer(
                    writer_state.clone(),
                    identity,
                    captured,
                    progress_receiver,
                    writer_memory,
                    writer_active,
                    target,
                )
                .await
                {
                    Ok(WriterOutcome::Sealed) => Ok(()),
                    // The writer owns cancellation even when begin commits
                    // after its producer disappears. A successful gap fence
                    // is not a failed database operation.
                    Ok(WriterOutcome::Abandoned) => {
                        fence_failed_capture(&writer_state, identity, target).await
                    }
                    Err(error) => {
                        failure_active.store(false, Ordering::Release);
                        let _ = fence_failed_capture(&writer_state, identity, target).await;
                        Err(error)
                    }
                }
            },
            "response_spool_writer",
            Some(active.clone()),
        );
        Self {
            identity,
            #[cfg(test)]
            state: state.clone(),
            capture: CarrierCaptureInput {
                budget: state
                    .persistence
                    .stream_memory
                    .reservation()
                    .shared_budget(),
                source: Some(source),
                progress,
                length: 0,
                #[cfg(test)]
                write_pause: None,
                #[cfg(test)]
                owner: Arc::downgrade(&producer_owner),
            },
            writer,
            active,
            _queue_memory: queue_memory,
        }
    }

    #[cfg(test)]
    pub(super) async fn begin_for_test(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
    ) -> Result<ResponseArchiveWriter, AppError> {
        ResponseArchiveWriter::begin_inner(state.clone(), identity).await
    }

    /// Hand each chunk to the shared carrier without an archive-admission gate.
    /// Storage is needed only when real RAM credit cannot retain the source.
    pub(crate) async fn append_with_backpressure(&mut self, chunk: Bytes) -> bool {
        #[cfg(test)]
        if take_append_failure_for_test(&self.state) {
            self.abandon("injected_append_failure");
            return false;
        }
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        if self.capture.append(chunk).await.is_ok() {
            true
        } else {
            self.abandon("carrier_capture_unavailable");
            false
        }
    }

    /// Nonblocking archival offer. Failure fences this incomplete capture;
    /// accepted chunks remain ordered in the existing single carrier queue.
    pub(crate) fn try_offer(&mut self, bytes: Bytes) -> bool {
        if bytes.is_empty() {
            return self.active.load(Ordering::Acquire);
        }
        // Account channel/receipt/allocation metadata as well as wire bytes.
        let Some(charged) = bytes.len().checked_add(256) else {
            self.abandon("capture_byte_budget_unavailable");
            return false;
        };
        let Some(credit) = self.capture.budget.try_reserve_ingress_bytes(charged) else {
            self.abandon("capture_byte_budget_unavailable");
            return false;
        };
        let (acknowledged, _completion) = tokio::sync::oneshot::channel();
        let length = bytes.len() as u64;
        let chunk = CaptureChunk {
            bytes,
            credit: Some(credit),
            acknowledged,
            #[cfg(test)]
            pause: self.capture.write_pause.take(),
        };
        let accepted = self
            .capture
            .source
            .as_ref()
            .is_some_and(|source| source.send(chunk).is_ok());
        if !accepted {
            self.abandon("capture_queue_unavailable");
            return false;
        }
        self.capture.length = self.capture.length.saturating_add(length);
        self.capture
            .progress
            .send_replace((self.capture.length, false));
        true
    }

    fn abandon(&mut self, reason: &'static str) {
        tracing::warn!(request_id = %self.identity.request_id, phase = "response_spool_producer", outcome = reason, "response capture abandoned");
        self.active.store(false, Ordering::Release);
        self.capture.source.take();
        self.capture
            .progress
            .send_replace((self.capture.length, true));
    }

    #[cfg(test)]
    pub(crate) fn local_source_owner_for_test(&self) -> std::sync::Weak<()> {
        self.capture.owner.clone()
    }

    #[cfg(test)]
    pub(super) fn pause_local_write_for_test(
        &mut self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered, entering) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        self.capture.write_pause = Some((entered, released));
        (entering, release)
    }

    #[cfg(test)]
    pub(super) async fn cancel_and_wait_for_test(mut self) -> Option<Result<(), AppError>> {
        self.abandon("test_cancel");
        self.writer.wait().await
    }

    /// Capture EOF is not an ACK. The owned writer must still publish its CAS root.
    pub(crate) fn seal(mut self) -> Option<ResponseArchiveSettlement> {
        if !self.active.load(Ordering::Acquire) {
            return None;
        }
        // Publish EOF before closing an empty source: the receiver can wake
        // concurrently as soon as its sender is dropped.
        self.capture
            .progress
            .send_replace((self.capture.length, true));
        self.capture.source.take();
        self.writer.continue_on_drop();
        Some(ResponseArchiveSettlement {
            _writer: self.writer,
        })
    }
}

#[cfg(test)]
impl ResponseArchiveWriter {
    async fn begin_inner(
        state: AppState,
        identity: ArchiveSpoolIdentity,
    ) -> Result<Self, AppError> {
        let compression_enabled = state.config.archive_spool_compression_enabled;
        if !observe_writer_database(
            identity,
            "response_spool_begin",
            0,
            state.db.begin_response_archive_spool(identity),
        )
        .await?
        {
            return Err(AppError::internal("producer_begin_inner_1"));
        }
        Ok(Self {
            state,
            identity,
            compression_enabled,
            seq: 0,
            bytes: 0,
            append_attempts: 0,
            append_wait: std::time::Duration::ZERO,
            max_append_wait: std::time::Duration::ZERO,
        })
    }

    async fn append_chunks(&mut self, chunks: Vec<QueuedArchiveChunk>) -> Result<(), AppError> {
        if chunks.is_empty() || chunks.len() > super::CAPTURE_DATABASE_BATCH_CHUNKS {
            return Err(AppError::internal("producer_append_chunks_1"));
        }
        let chunk_count = chunks.len();
        let batch_bytes = chunks.iter().try_fold(0_i64, |total, chunk| {
            total
                .checked_add(
                    i64::try_from(chunk.bytes.len())
                        .map_err(|_| AppError::internal("producer_append_chunks_1"))?,
                )
                .ok_or(AppError::internal("producer_append_chunks_2"))
        })?;
        let mut chunks = chunks.into_iter();
        let identity = self.identity;
        let compression_enabled = self.compression_enabled;
        let pepper = self.state.config.key_pepper.clone();
        let appended = {
            let append_started = std::time::Instant::now();
            // The database requests one chunk at a time and inserts it before
            // requesting the next. Sealing therefore replaces one permitted
            // plaintext with one transient ciphertext; it never retains four
            // plaintexts and four ciphertexts together. Once the plaintext
            // permit is released, a producer refill remains inside the same
            // four-complete-chunk envelope.
            let result = observe_writer_database(
                identity,
                "response_spool_append",
                batch_bytes,
                self.state.db.append_response_archive_spool_batch_with(
                    identity,
                    self.seq,
                    chunk_count,
                    move |seq| {
                        let QueuedArchiveChunk {
                            bytes,
                            _memory: memory,
                        } =
                            chunks.next().ok_or(AppError::internal("producer_append_chunks_3"))?;
                        let byte_count =
                            i64::try_from(bytes.len()).map_err(|_| AppError::internal("producer_append_chunks_2"))?;
                        let ciphertext = super::cipher::seal_for_purpose_with_compression(
                            identity,
                            seq,
                            &bytes,
                            pepper.as_bytes(),
                            super::BufferedArchivePurpose::Response,
                            compression_enabled,
                        )
                        .inspect_err(|error| {
                            tracing::warn!(request_id = %identity.request_id, phase = "response_spool_encrypt", error_category = error.diagnostic_category(), "response archive chunk encryption failed");
                        })?;
                        drop(bytes);
                        drop(memory);
                        Ok(crate::db::ArchiveSpoolChunk {
                            seq,
                            byte_count,
                            ciphertext,
                        })
                    },
                ),
            )
            .await;
            let elapsed = append_started.elapsed();
            self.append_wait += elapsed;
            self.max_append_wait = self.max_append_wait.max(elapsed);
            result
        };
        self.append_attempts += 1;
        if !appended? {
            return Err(AppError::internal("producer_append_chunks_4"));
        }
        self.seq = self
            .seq
            .checked_add(
                i64::try_from(chunk_count)
                    .map_err(|_| AppError::internal("producer_append_chunks_3"))?,
            )
            .ok_or(AppError::internal("producer_append_chunks_5"))?;
        self.bytes = self
            .bytes
            .checked_add(batch_bytes)
            .ok_or(AppError::internal("producer_append_chunks_6"))?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn append_for_test(&mut self, chunks: Vec<Bytes>) -> Result<(), AppError> {
        let mut buffered = Vec::with_capacity(super::CHUNK_BYTES);
        let mut batch = Vec::with_capacity(super::CAPTURE_DATABASE_BATCH_CHUNKS);
        for chunk in chunks {
            let mut remaining = chunk.as_ref();
            while !remaining.is_empty() {
                let take = remaining.len().min(super::CHUNK_BYTES - buffered.len());
                buffered.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if buffered.len() == super::CHUNK_BYTES {
                    batch.push(QueuedArchiveChunk {
                        bytes: Bytes::from(std::mem::take(&mut buffered)),
                        _memory: None,
                    });
                    if batch.len() == super::CAPTURE_DATABASE_BATCH_CHUNKS {
                        self.append_chunks(std::mem::take(&mut batch)).await?;
                        batch = Vec::with_capacity(super::CAPTURE_DATABASE_BATCH_CHUNKS);
                    }
                }
            }
        }
        if !buffered.is_empty() {
            batch.push(QueuedArchiveChunk {
                bytes: Bytes::from(buffered),
                _memory: None,
            });
        }
        if !batch.is_empty() {
            self.append_chunks(batch).await?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn append_state_for_test(&self) -> (u64, i64, i64) {
        (self.append_attempts, self.seq, self.bytes)
    }

    async fn seal_inner(self) -> Result<(), AppError> {
        if !observe_writer_database(
            self.identity,
            "response_spool_seal",
            self.bytes,
            self.state
                .db
                .seal_response_archive_spool(self.identity, self.seq, self.bytes),
        )
        .await?
        {
            return Err(AppError::internal("producer_seal_inner_1"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn seal_for_test(self) -> Result<(), AppError> {
        self.seal_inner().await
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WriterOutcome {
    Sealed,
    Abandoned,
}

async fn captured_source(
    source: &mut super::OwnedTask<CapturedResponse>,
    identity: ArchiveSpoolIdentity,
    progress: tokio::sync::watch::Receiver<(u64, bool)>,
    active: Arc<AtomicBool>,
) -> Result<Option<CapturedResponse>, AppError> {
    tokio::select! {
        result = source.wait() => match result {
            Some(Ok(source)) if active.load(Ordering::Acquire) => Ok(Some(source)),
            Some(Err(error)) if active.load(Ordering::Acquire) => Err(error),
            _ => { writer_interrupted(identity, "carrier_source_dropped"); Ok(None) }
        },
        () = capture_abandoned(progress, active.clone()) => Ok(None),
    }
}

// Closing an unsealed producer or explicitly abandoning it wakes resource
// waits. A sealed source remains owned even after every observer disappears.
async fn capture_abandoned(
    mut progress: tokio::sync::watch::Receiver<(u64, bool)>,
    active: Arc<AtomicBool>,
) {
    loop {
        if !active.load(Ordering::Acquire) {
            return;
        }
        if progress.changed().await.is_err() {
            if !progress.borrow().1 || !active.load(Ordering::Acquire) {
                return;
            }
            std::future::pending::<()>().await;
        }
    }
}

/// The common carrier remains the complete retry source. A separate, already
/// reserved upload envelope owns multipart copies instead of source-pool leases.
async fn stage_captured_response(
    state: &AppState,
    source: &Arc<crate::gateway_body::carrier::BodyCarrier>,
    staging: crate::archive_staging::ArchiveStagingKey,
    workspace: Option<Arc<CaptureQueueMemory>>,
) -> Result<
    (
        crate::archive::StagedArchiveObject,
        Option<crate::archive::CarrierObjectOwner>,
    ),
    AppError,
> {
    if let Some(staged) = source.sealed_staged_reference() {
        return Ok((staged.clone(), None));
    }
    let locator = format!(
        "{}/carrier-{}",
        staging.canonical_prefix(),
        uuid::Uuid::now_v7()
    );
    let owner = crate::archive::CarrierObjectOwner::from_identity(
        staging,
        locator.clone(),
        source.len(),
        source.digest().to_owned(),
    )?;
    // An unknown receipt acknowledgement must never authorize a body upload.
    state.archive.begin_carrier_object(&owner).await?;
    let mut writer = state
        .archive
        .start_writer(&locator)
        .await
        .map_err(|error| error.storage_phase("multipart_begin"))?;
    if let Some(workspace) = &workspace {
        writer.retain_workspace(workspace.clone());
    }
    use futures_util::StreamExt;
    let mut stream = match workspace {
        Some(workspace) => source.open_stream_with_workspace(workspace),
        None => source.open_stream(),
    };
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.storage_phase("source_read"))?;
        // Preserve the source allocation owner through SDK consumption rather
        // than copying an arbitrarily large credited segment into workspace.
        if let Err(error) = writer.write(chunk).await {
            let _ = writer.abort().await;
            return Err(error.storage_phase("multipart_write"));
        }
    }
    drop(stream);
    let staged = writer
        .finish_staged()
        .await
        .map_err(|error| error.storage_phase("multipart_complete"))?;
    if staged.size_bytes != source.len() || staged.blake3_digest != source.digest() {
        return Err(AppError::storage(
            "response_carrier_stage_identity_mismatch",
        ));
    }
    Ok((staged, Some(owner)))
}

/// Join the same FIFO as request uploads. Never hold one scarce resource
/// while waiting for the other: an unavailable upload slot releases workspace
/// before waiting for a wakeup, then rejoins the workspace queue.
async fn response_upload_workspace(state: &AppState) -> Result<Arc<CaptureQueueMemory>, AppError> {
    loop {
        let workspace = state
            .persistence
            .multipart_workspace(crate::api::proxy::persistence::WorkspaceRole::ResponseStage)
            .await?;
        match state
            .proxy_archive_stream_permits
            .clone()
            .try_acquire_owned()
        {
            Ok(permit) => {
                return Ok(Arc::new(CaptureQueueMemory {
                    _local_metrics: None,
                    _archive_permit: Some(permit),
                    reservation: None,
                    workspace: Some(workspace),
                }));
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(AppError::internal("capture_upload_admission_closed"));
            }
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                drop(workspace);
                let wakeup = state
                    .proxy_archive_stream_permits
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| AppError::internal("capture_upload_admission_closed"))?;
                drop(wakeup);
            }
        }
    }
}

async fn run_response_archive_writer(
    mut state: AppState,
    identity: ArchiveSpoolIdentity,
    mut source_task: super::OwnedTask<CapturedResponse>,
    progress: tokio::sync::watch::Receiver<(u64, bool)>,
    memory: Arc<CaptureQueueMemory>,
    active: Arc<AtomicBool>,
    target: CaptureTarget,
) -> Result<WriterOutcome, AppError> {
    // Upload concurrency and multipart memory belong to this background task,
    // never to admission of the supplier/client stream. Shared carrier capture
    // runs independently; ordered ingress is bounded by archival byte credit.
    #[cfg(test)]
    pause_begin_for_test(&state).await;
    #[cfg(test)]
    pause_begin_ack_for_test(&state).await;
    let Some(captured) =
        captured_source(&mut source_task, identity, progress, active.clone()).await?
    else {
        return Ok(WriterOutcome::Abandoned);
    };
    let source = captured.source;
    let publication_started = std::time::Instant::now();
    let mut publication_attempt = 0_u64;
    let (staged, staging_owner) = loop {
        let staged_attempt = async {
            let _upload_memory = if memory._archive_permit.is_none() {
                Some(response_upload_workspace(&state).await?)
            } else {
                None
            };
            let _upload_metrics = state.metrics.memory_usage(
                crate::metrics::MemoryComponent::StreamCapture,
                super::CAPTURE_MEMORY_BYTES,
            );
            stage_captured_response(&state, &source, captured.staging, _upload_memory).await
        }
        .await;
        match staged_attempt {
            Ok(staged) => break staged,
            Err(error @ AppError::Storage { .. }) => {
                publication_attempt = publication_attempt.saturating_add(1);
                let kind = match &error {
                    AppError::Storage { facts, .. } => facts.kind.unwrap_or("operation_error"),
                    _ => "operation_error",
                };
                tracing::warn!(request_id = %identity.request_id, error_category = error.diagnostic_category(),
                    stage = "carrier_stage", error_kind = kind,
                    phase = match &error { AppError::Storage { facts, .. } => facts.phase, _ => None },
                    storage_status = match &error { AppError::Storage { facts, .. } => facts.status, _ => None },
                    backend = if captured.staging.backend_identity.is_some() { "training" } else { "legacy" },
                    attempt = publication_attempt,
                    elapsed_ms = publication_started.elapsed().as_millis() as u64,
                    "complete carrier source awaits object storage publication");
                let delay = crate::archive_staging::cleanup_backoff_with_jitter(
                    identity.request_id,
                    publication_attempt.min(u32::MAX as u64) as u32,
                )
                .min(30_000);
                tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
            }
            Err(error) => return Err(error),
        }
    };
    let object = loop {
        match state
            .archive
            .promote_staged_text_to_cas(identity.tenant_id, &staged)
            .await
        {
            Ok(object) => break object,
            Err(error @ AppError::Storage { .. }) => {
                publication_attempt = publication_attempt.saturating_add(1);
                let kind = match &error {
                    AppError::Storage { facts, .. } => facts.kind.unwrap_or("operation_error"),
                    _ => "operation_error",
                };
                tracing::warn!(request_id = %identity.request_id, error_category = error.diagnostic_category(),
                    stage = "cas_promote", error_kind = kind, attempt = publication_attempt,
                    elapsed_ms = publication_started.elapsed().as_millis() as u64,
                    "sealed original awaits immutable CAS promotion");
                let delay = crate::archive_staging::cleanup_backoff_with_jitter(
                    identity.request_id,
                    publication_attempt.min(u32::MAX as u64) as u32,
                )
                .min(30_000);
                tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
            }
            Err(error) => return Err(error),
        }
    };
    let publication = loop {
        match target.publish(&state.archive, identity, &object).await {
            Ok(publication) => break publication,
            Err(error @ AppError::Storage { .. }) => {
                publication_attempt = publication_attempt.saturating_add(1);
                let kind = match &error {
                    AppError::Storage { facts, .. } => facts.kind.unwrap_or("operation_error"),
                    _ => "operation_error",
                };
                tracing::warn!(request_id = %identity.request_id, error_category = error.diagnostic_category(),
                    stage = "root_publish", error_kind = kind, attempt = publication_attempt,
                    elapsed_ms = publication_started.elapsed().as_millis() as u64,
                    "immutable original awaits durable root publication");
                let delay = crate::archive_staging::cleanup_backoff_with_jitter(
                    identity.request_id,
                    publication_attempt.min(u32::MAX as u64) as u32,
                )
                .min(30_000);
                tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
            }
            Err(error) => return Err(error),
        }
    };
    // The immutable CAS object is confirmed. Publication either owns a raw
    // root or has transferred responsibility to verified semantic retirement.
    // Old readers retain their original allocation leases in either case.
    source.handoff_verified_publication(state.archive.clone(), object.clone())?;
    if let Some(owner) = staging_owner
        && let Err(error) = state.archive.release_carrier_object(&owner).await
    {
        tracing::warn!(request_id = %identity.request_id, error_category = error.diagnostic_category(),
            "complete original retains its exact staging cleanup receipt");
    }
    if let crate::archive::RawPublicationSettlement::Retired {
        semantic_revision,
        semantic_leaf,
    } = publication
    {
        tracing::debug!(request_id = %identity.request_id, semantic_revision, %semantic_leaf,
            "late response publisher transferred responsibility to verified semantic retirement");
        // Retirement is not a new raw-root acknowledgement. In particular, a
        // late publisher must not restore the retired SQL locator binding.
        return Ok(WriterOutcome::Sealed);
    }
    if matches!(target, CaptureTarget::DiscardedAttempt(_)) {
        return Ok(WriterOutcome::Sealed);
    }
    state.db.refresh_closed_persistence_pool();
    if let Err(error) = state
        .db
        .bind_raw_capture(identity.tenant_id, identity.request_id, "response", &object)
        .await
    {
        // The immutable root is already the durable authority. The body
        // worker retries relational binding from that root; a metadata outage
        // must not relabel this complete original as a failed capture.
        tracing::warn!(request_id = %identity.request_id, error_category = error.diagnostic_category(),
            "raw response root awaits relational projection");
    }
    Ok(WriterOutcome::Sealed)
}

#[cfg(test)]
async fn mark_gap_inner(
    state: AppState,
    identity: ArchiveSpoolIdentity,
    reason: &'static str,
) -> Result<(), AppError> {
    super::fence_probe::observe(&state).await;
    observe_writer_database(identity, "response_spool_gap_write", 0, async {
        state
            .db
            .fail_response_archive_spool(identity, reason)
            .await
            .map(|()| true)
    })
    .await
    .map(|_| ())
}

fn writer_interrupted(identity: ArchiveSpoolIdentity, reason: &'static str) -> WriterOutcome {
    tracing::warn!(request_id = %identity.request_id, phase = "response_spool_writer", outcome = reason, "response writer was abandoned before durable completion");
    WriterOutcome::Abandoned
}

#[cfg(test)]
async fn observe_writer_database(
    identity: ArchiveSpoolIdentity,
    phase: &'static str,
    bytes: i64,
    operation: impl std::future::Future<Output = Result<bool, AppError>>,
) -> Result<bool, AppError> {
    let started = std::time::Instant::now();
    if phase == "response_spool_append" {
        tracing::debug!(request_id = %identity.request_id, phase, outcome = "started", bytes, "response archive database operation");
    } else {
        tracing::info!(request_id = %identity.request_id, phase, outcome = "started", bytes, "response archive database operation");
    }
    // The existing SQLx conversion emits only a safe fixed error_kind (for
    // example pool_timeout/query_cancelled). Keep that event correlated before
    // conversion erases its type into AppError::internal("producer_observe_writer_database_1").
    let result = operation.instrument(tracing::info_span!("response_archive_database", request_id = %identity.request_id, phase)).await;
    let outcome = match &result {
        Ok(true) => "acknowledged",
        Ok(false) => "rejected",
        Err(_) => "database_error",
    };
    match &result {
        Ok(true) if phase == "response_spool_append" => {
            tracing::debug!(request_id = %identity.request_id, phase, outcome,
                elapsed_ms = started.elapsed().as_millis() as u64, bytes,
                "response archive database operation");
        }
        Ok(true) => {
            tracing::info!(request_id = %identity.request_id, phase, outcome,
                elapsed_ms = started.elapsed().as_millis() as u64, bytes,
                "response archive database operation");
        }
        _ => {
            tracing::warn!(request_id = %identity.request_id, phase, outcome,
                error_category = result.as_ref().err().map(AppError::diagnostic_category),
                elapsed_ms = started.elapsed().as_millis() as u64, bytes,
                "response archive database operation");
        }
    }
    result
}

#[cfg(test)]
pub(super) async fn mark_gap_for_test(
    state: &AppState,
    identity: ArchiveSpoolIdentity,
    reason: &'static str,
) -> Result<(), AppError> {
    mark_gap_inner(state.clone(), identity, reason).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn unreached_preseal_registration_is_cleaned_on_timeout_failure_and_cancellation() {
        let request_id = uuid::Uuid::new_v4();
        let other_id = uuid::Uuid::new_v4();
        let (_other_entered, _other_release, _other_registration) =
            scoped_request_preseal_pause_for_test(other_id);
        let assert_cleanup = || {
            let pauses = PAUSE_REQUEST_PRESEAL.lock().unwrap();
            assert!(!pauses.contains_key(&request_id));
            assert!(pauses.contains_key(&other_id));
        };

        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let (entered, _release, _registration) =
                scoped_request_preseal_pause_for_test(request_id);
            entered.await.unwrap();
        })
        .await;
        assert!(timed_out.is_err());
        assert_cleanup();

        let failed = std::panic::catch_unwind(|| {
            let (_entered, _release, _registration) =
                scoped_request_preseal_pause_for_test(request_id);
            panic!("synthetic fixture failure before preseal entry");
        });
        assert!(failed.is_err());
        assert_cleanup();

        let (registered, registration) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (entered, _release, _registration) =
                scoped_request_preseal_pause_for_test(request_id);
            registered.send(()).unwrap();
            entered.await.unwrap();
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), registration)
            .await
            .expect("bounded registration before cancellation")
            .unwrap();
        task.abort();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .expect("bounded cancelled registration task")
                .unwrap_err()
                .is_cancelled()
        );
        assert_cleanup();
    }

    #[tokio::test]
    async fn consumed_preseal_guard_does_not_remove_a_replacement_registration() {
        let request_id = uuid::Uuid::new_v4();
        let (mut entered, release, original) = scoped_request_preseal_pause_for_test(request_id);
        release.send(()).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            pause_request_preseal_for_test(request_id),
        )
        .await
        .expect("bounded original preseal entry");
        entered.try_recv().unwrap();
        let (_entered, _release, replacement) = scoped_request_preseal_pause_for_test(request_id);
        drop(original);
        assert!(
            PAUSE_REQUEST_PRESEAL
                .lock()
                .unwrap()
                .contains_key(&request_id)
        );
        drop(replacement);
        assert!(
            !PAUSE_REQUEST_PRESEAL
                .lock()
                .unwrap()
                .contains_key(&request_id)
        );
    }

    #[test]
    fn writer_observation_preserves_results_and_correlates_safe_database_errors() {
        use futures_util::FutureExt;
        use std::sync::Mutex;
        #[derive(Clone, Default)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = Writer::default();
        let sink = writer.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for accepted in [true, false] {
                let result =
                    observe_writer_database(identity(), "response_spool_begin", 0, async {
                        Ok(accepted)
                    })
                    .now_or_never()
                    .unwrap()
                    .unwrap();
                assert_eq!(result, accepted);
            }
            let result = observe_writer_database(identity(), "response_spool_append", 8, async {
                Err(sqlx::Error::PoolTimedOut.into())
            })
            .now_or_never()
            .unwrap();
            assert!(matches!(
                result,
                Err(AppError::Capacity {
                    code: "database_pool_exhausted",
                    ..
                })
            ));
            let result = observe_writer_database(identity(), "response_spool_seal", 8, async {
                Err(AppError::storage("object_storage_failed"))
            })
            .now_or_never()
            .unwrap();
            assert!(matches!(result, Err(AppError::Storage { .. })));
        });
        let bytes = writer.0.lock().unwrap();
        let logs = std::str::from_utf8(&bytes).unwrap();
        assert!(!logs.contains("SECRET_CANARY"));
        let events: Vec<serde_json::Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let error = events
            .iter()
            .find(|event| event["fields"]["error_kind"] == "pool_timeout")
            .unwrap();
        assert_eq!(
            error["span"]["request_id"],
            identity().request_id.to_string()
        );
        assert_eq!(error["span"]["phase"], "response_spool_append");
        let outcomes: Vec<_> = events
            .iter()
            .filter_map(|event| event["fields"]["outcome"].as_str())
            .collect();
        assert!(outcomes.contains(&"acknowledged"));
        assert!(outcomes.contains(&"rejected"));
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == "database_error")
                .count(),
            2
        );
    }

    fn identity() -> ArchiveSpoolIdentity {
        ArchiveSpoolIdentity {
            request_id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            reservation_id: uuid::Uuid::nil(),
        }
    }

    #[test]
    fn buffered_archive_replays_its_body_but_new_captures_get_fresh_nonces() {
        let body = Bytes::from(
            serde_json::to_vec(&vec![
                serde_json::json!({"content":"synthetic repeat for fixed nonce replay"});
                400
            ])
            .unwrap(),
        );
        let first = BufferedArchive::new(
            identity(),
            super::super::BufferedArchivePurpose::Response,
            &body,
            b"archive-spool-test-pepper",
            true,
        )
        .unwrap();
        let second = BufferedArchive::new(
            identity(),
            super::super::BufferedArchivePurpose::Response,
            &body,
            b"archive-spool-test-pepper",
            true,
        )
        .unwrap();

        let replay = first.seal(0).unwrap();
        assert!(replay.starts_with("zstd1."));
        assert_eq!(replay, first.seal(0).unwrap());
        assert_ne!(first.seal(0).unwrap(), second.seal(0).unwrap());
    }

    #[test]
    fn raw_json_above_former_plain_ceiling_is_chunked_in_full() {
        const FORMER_PLAIN_CEILING: usize = 64 * 1024 * 1024;
        let chunk_bytes = super::super::CHUNK_BYTES;
        assert_eq!(FORMER_PLAIN_CEILING % chunk_bytes, 0);
        let mut raw = vec![b'x'; FORMER_PLAIN_CEILING + 1];
        raw[0] = b'a';
        raw[FORMER_PLAIN_CEILING] = b'z';
        let body = Bytes::from(raw);
        let pepper = b"archive-spool-test-pepper";
        let chunk_count = body.len().div_ceil(chunk_bytes);
        let tail_seq = chunk_count - 1;
        let tail_len = body.len() - tail_seq * chunk_bytes;
        assert_eq!(chunk_count, FORMER_PLAIN_CEILING / chunk_bytes + 1);
        assert_eq!(tail_len, 1);

        let request = super::super::BufferedArchivePurpose::Request;
        let archive = match BufferedArchive::new(identity(), request, &body, pepper, false) {
            Ok(archive) => archive,
            Err(error) => panic!("raw request JSON capture failed: {error}"),
        };
        let opened_tail = super::super::cipher::open_for_purpose(
            identity(),
            tail_seq as i64,
            &archive.seal(tail_seq).unwrap(),
            tail_len as i64,
            pepper,
            request,
        )
        .unwrap();
        assert_eq!(opened_tail.as_ref(), &body[FORMER_PLAIN_CEILING..]);
        assert!(archive.seal(chunk_count).is_err());
        let opened_first = super::super::cipher::open_for_purpose(
            identity(),
            0,
            &archive.seal(0).unwrap(),
            chunk_bytes as i64,
            pepper,
            request,
        )
        .unwrap();
        assert_eq!(opened_first.len(), chunk_bytes);
        assert_eq!(opened_first[0], body[0]);
        assert_eq!(&opened_first[1..], &body[1..chunk_bytes]);

        let response = super::super::BufferedArchivePurpose::Response;
        let chunks = match encrypt_buffered(identity(), response, &body, pepper) {
            Ok(chunks) => chunks,
            Err(error) => panic!("raw response JSON capture failed: {error}"),
        };
        assert_eq!(chunks.len(), chunk_count);
        assert!(chunks.iter().enumerate().all(|(seq, chunk)| {
            let start = seq * chunk_bytes;
            chunk.seq == seq as i64
                && chunk.byte_count as usize == body.len().saturating_sub(start).min(chunk_bytes)
        }));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.byte_count as usize)
                .sum::<usize>(),
            body.len()
        );
        let opened_response_tail = super::super::cipher::open_for_purpose(
            identity(),
            tail_seq as i64,
            &chunks[tail_seq].ciphertext,
            tail_len as i64,
            pepper,
            response,
        )
        .unwrap();
        assert_eq!(opened_response_tail.as_ref(), &body[FORMER_PLAIN_CEILING..]);
        let opened_response_first = super::super::cipher::open_for_purpose(
            identity(),
            0,
            &chunks[0].ciphertext,
            chunk_bytes as i64,
            pepper,
            response,
        )
        .unwrap();
        assert_eq!(opened_response_first.as_ref(), &body[..chunk_bytes]);
    }

    #[tokio::test(start_paused = true)]
    async fn acknowledgement_deadline_fails_closed_without_cancelling_the_operation() {
        let ack_timeout = std::time::Duration::from_millis(250);
        let (release, released) = tokio::sync::oneshot::channel();
        let (settled, settlement) = tokio::sync::oneshot::channel();
        let started = tokio::time::Instant::now();
        let result = super::super::await_owned(
            ack_timeout,
            async move {
                released.await.unwrap();
                settled.send(()).unwrap();
                Ok(())
            },
            "response_spool_ack_test",
        )
        .await;

        assert!(result.is_none());
        assert_eq!(started.elapsed(), ack_timeout);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), settlement)
            .await
            .expect("timed-out acknowledgement must settle in its owned task")
            .unwrap();
    }
}

#[cfg(test)]
#[path = "workspace_fairness_contracts.rs"]
mod workspace_fairness_contracts;
