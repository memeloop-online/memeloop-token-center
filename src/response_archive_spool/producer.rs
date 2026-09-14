use bytes::Bytes;
use getrandom::fill;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{AppState, db::ArchiveSpoolIdentity, error::AppError};

// Deliberately no Debug: prepared ciphertext must not enter logs.
pub(crate) struct PreparedArchiveBatch {
    identity: ArchiveSpoolIdentity,
    purpose: super::BufferedArchivePurpose,
    chunks: Vec<crate::db::ArchiveSpoolChunk>,
}

impl PreparedArchiveBatch {
    pub(crate) fn into_chunks_for(
        self,
        archive: &BufferedArchive<'_>,
    ) -> Option<Vec<crate::db::ArchiveSpoolChunk>> {
        (self.identity == archive.identity && self.purpose == archive.purpose)
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
    nonces: Vec<[u8; 12]>,
}

impl<'a> BufferedArchive<'a> {
    pub(crate) fn new(
        identity: ArchiveSpoolIdentity,
        purpose: super::BufferedArchivePurpose,
        body: &'a Bytes,
        pepper: &'a [u8],
    ) -> Result<Self, AppError> {
        if body.len() > 64 * 1024 * 1024 {
            return Err(AppError::Overloaded);
        }
        let chunk_count = body.len().div_ceil(super::CHUNK_BYTES);
        let mut nonces = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            let mut nonce = [0_u8; 12];
            fill(&mut nonce).map_err(|_| AppError::Internal)?;
            nonces.push(nonce);
        }
        Ok(Self {
            identity,
            purpose,
            body,
            pepper,
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
        let nonce = self.nonces.get(seq).copied().ok_or(AppError::Internal)?;
        let start = seq
            .checked_mul(super::CHUNK_BYTES)
            .ok_or(AppError::Internal)?;
        let end = (start + super::CHUNK_BYTES).min(self.body.len());
        let bytes = self.body.get(start..end).ok_or(AppError::Internal)?;
        let sealed = super::cipher::seal_for_purpose_with_nonce(
            self.identity,
            i64::try_from(seq).map_err(|_| AppError::Internal)?,
            bytes,
            self.pepper,
            self.purpose,
            nonce,
        )?;
        debug_assert_eq!(Some(sealed.len()), self.sealed_len(bytes.len()));
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
                    .ok_or(AppError::Internal)?;
                let end = (start + super::CHUNK_BYTES).min(self.body.len());
                Ok(crate::db::ArchiveSpoolChunk {
                    seq: i64::try_from(seq).map_err(|_| AppError::Internal)?,
                    ciphertext: self.seal(seq)?,
                    byte_count: i64::try_from(end - start).map_err(|_| AppError::Internal)?,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        #[cfg(test)]
        pause_request_preseal_for_test(self.identity.request_id).await;
        Ok(PreparedArchiveBatch {
            identity: self.identity,
            purpose: self.purpose,
            chunks,
        })
    }
}

#[cfg(test)]
type RequestPresealPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
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
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let mut pauses = PAUSE_REQUEST_PRESEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(pauses.insert(request_id, (entered, released)).is_none());
    (entering, release)
}

#[cfg(test)]
async fn pause_request_preseal_for_test(request_id: uuid::Uuid) {
    let pause = PAUSE_REQUEST_PRESEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&request_id);
    if let Some((entered, released)) = pause {
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
    if body.len() > 64 * 1024 * 1024 {
        return Err(AppError::Overloaded);
    }
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
    state: AppState,
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: i64,
}

struct CaptureAck<T> {
    value: Option<T>,
    state: AppState,
    identity: ArchiveSpoolIdentity,
}

impl<T> CaptureAck<T> {
    fn new(value: T, state: AppState, identity: ArchiveSpoolIdentity) -> Self {
        Self {
            value: Some(value),
            state,
            identity,
        }
    }

    fn claim(mut self) -> T {
        self.value.take().expect("capture acknowledgement value")
    }
}

impl<T> Drop for CaptureAck<T> {
    fn drop(&mut self) {
        if self.value.is_none() {
            return;
        }
        let state = self.state.clone();
        let identity = self.identity;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = state
                    .db
                    .fail_response_archive_spool(identity, "capture_failed")
                    .await
                {
                    tracing::warn!(
                        stage = "response_spool_late_ack_fence",
                        error_category = error.diagnostic_category(),
                        "late response archive acknowledgement could not be fenced"
                    );
                }
            });
        }
    }
}

async fn fence_failed_capture(state: &AppState, identity: ArchiveSpoolIdentity) {
    if let Err(error) = state
        .db
        .fail_response_archive_spool(identity, "capture_failed")
        .await
    {
        tracing::warn!(
            stage = "response_spool_failed_ack_fence",
            error_category = error.diagnostic_category(),
            "failed response archive acknowledgement could not be fenced"
        );
    }
}

impl ResponseArchiveProducer {
    pub(crate) async fn begin(state: &AppState, identity: ArchiveSpoolIdentity) -> Option<Self> {
        let state = state.clone();
        let cleanup_state = state.clone();
        match bounded_ack(move |_| async move {
            let producer = match Self::begin_inner(state.clone(), identity).await {
                Ok(producer) => producer,
                Err(error) => {
                    fence_failed_capture(&state, identity).await;
                    return Err(error);
                }
            };
            #[cfg(test)]
            pause_begin_ack_for_test(&producer.state).await;
            Ok(CaptureAck::new(producer, cleanup_state, identity))
        })
        .await
        {
            Some(producer) => Some(producer),
            _ => {
                tracing::warn!(request_id = %identity.request_id, stage = "response_spool_admission", "proxy archive gap");
                None
            }
        }
    }

    async fn begin_inner(
        state: AppState,
        identity: ArchiveSpoolIdentity,
    ) -> Result<Self, AppError> {
        if !state.db.begin_response_archive_spool(identity).await? {
            return Err(AppError::Internal);
        }
        Ok(Self {
            state,
            identity,
            seq: 0,
            bytes: 0,
        })
    }

    #[cfg(test)]
    pub(super) async fn begin_for_test(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
    ) -> Result<Self, AppError> {
        Self::begin_inner(state.clone(), identity).await
    }

    pub(crate) async fn append(&mut self, chunks: Vec<Bytes>) -> bool {
        #[cfg(test)]
        if take_append_failure_for_test(&self.state) {
            return false;
        }
        let state = self.state.clone();
        let identity = self.identity;
        let seq = self.seq;
        let bytes = self.bytes;
        let cleanup_state = state.clone();
        match bounded_ack(move |active| async move {
            let _memory = state.metrics.memory_usage(
                crate::metrics::MemoryComponent::StreamCapture,
                super::CHUNK_BYTES * 5,
            );
            let counters =
                match Self::append_inner(state.clone(), identity, seq, bytes, chunks, active).await
                {
                    Ok(counters) => counters,
                    Err(error) => {
                        fence_failed_capture(&state, identity).await;
                        return Err(error);
                    }
                };
            Ok(CaptureAck::new(counters, cleanup_state, identity))
        })
        .await
        {
            Some((seq, bytes)) => {
                self.seq = seq;
                self.bytes = bytes;
                true
            }
            None => false,
        }
    }

    async fn append_inner(
        state: AppState,
        identity: ArchiveSpoolIdentity,
        mut seq: i64,
        mut total_bytes: i64,
        chunks: Vec<Bytes>,
        active: Arc<AtomicBool>,
    ) -> Result<(i64, i64), AppError> {
        let mut buffered = Vec::with_capacity(super::CHUNK_BYTES);
        for chunk in chunks {
            let mut remaining = chunk.as_ref();
            while !remaining.is_empty() {
                let take = remaining.len().min(super::CHUNK_BYTES - buffered.len());
                buffered.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if buffered.len() == super::CHUNK_BYTES {
                    if !active.load(Ordering::Acquire) {
                        return Err(AppError::Internal);
                    }
                    Self::append_chunk(&state, identity, &mut seq, &mut total_bytes, &buffered)
                        .await?;
                    buffered.clear();
                }
            }
        }
        if !buffered.is_empty() {
            if !active.load(Ordering::Acquire) {
                return Err(AppError::Internal);
            }
            Self::append_chunk(&state, identity, &mut seq, &mut total_bytes, &buffered).await?;
        }
        Ok((seq, total_bytes))
    }

    #[cfg(test)]
    pub(super) async fn append_for_test(&mut self, chunks: Vec<Bytes>) -> Result<(), AppError> {
        let (seq, bytes) = Self::append_inner(
            self.state.clone(),
            self.identity,
            self.seq,
            self.bytes,
            chunks,
            Arc::new(AtomicBool::new(true)),
        )
        .await?;
        self.seq = seq;
        self.bytes = bytes;
        Ok(())
    }

    async fn append_chunk(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
        seq: &mut i64,
        total_bytes: &mut i64,
        bytes: &[u8],
    ) -> Result<(), AppError> {
        let ciphertext =
            super::cipher::seal(identity, *seq, bytes, state.config.key_pepper.as_bytes())?;
        let byte_count = i64::try_from(bytes.len()).map_err(|_| AppError::Internal)?;
        if !state
            .db
            .append_response_archive_spool(identity, *seq, byte_count, &ciphertext)
            .await?
        {
            return Err(AppError::Internal);
        }
        *seq += 1;
        *total_bytes += byte_count;
        Ok(())
    }

    pub(crate) async fn seal(self) -> bool {
        let Self {
            state,
            identity,
            seq,
            bytes,
        } = self;
        let cleanup_state = state.clone();
        bounded_ack(move |_| async move {
            if let Err(error) = Self::seal_inner(state.clone(), identity, seq, bytes).await {
                fence_failed_capture(&state, identity).await;
                return Err(error);
            }
            Ok(CaptureAck::new((), cleanup_state, identity))
        })
        .await
        .is_some()
    }

    async fn seal_inner(
        state: AppState,
        identity: ArchiveSpoolIdentity,
        seq: i64,
        bytes: i64,
    ) -> Result<(), AppError> {
        if !state
            .db
            .seal_response_archive_spool(identity, seq, bytes)
            .await?
        {
            return Err(AppError::Internal);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn seal_for_test(self) -> Result<(), AppError> {
        let Self {
            state,
            identity,
            seq,
            bytes,
        } = self;
        Self::seal_inner(state, identity, seq, bytes).await
    }
}

async fn bounded_ack<T, Operation, Ack>(operation: Operation) -> Option<T>
where
    T: Send + 'static,
    Operation: FnOnce(Arc<AtomicBool>) -> Ack,
    Ack: std::future::Future<Output = Result<CaptureAck<T>, AppError>> + Send + 'static,
{
    let active = Arc::new(AtomicBool::new(true));
    let result = super::await_owned_with_active(
        super::ACK_TIMEOUT,
        operation(active.clone()),
        "response_spool_ack",
        Some(active),
    )
    .await?;
    result.ok().map(CaptureAck::claim)
}

pub(crate) async fn mark_gap(
    state: &AppState,
    identity: ArchiveSpoolIdentity,
    reason: &'static str,
) {
    // Lost database ACKs are not retried by resending upstream or recharging.
    // Stale unsealed captures are independently fenced and expired by worker.
    let state = state.clone();
    let result = super::await_owned(
        super::ACK_TIMEOUT,
        async move { mark_gap_inner(state, identity, reason).await },
        "response_spool_gap_ack",
    )
    .await
    .and_then(Result::ok);
    if result.is_none() {
        tracing::warn!(request_id = %identity.request_id, stage = "response_spool_gap_ack", "proxy archive gap");
    }
}

async fn mark_gap_inner(
    state: AppState,
    identity: ArchiveSpoolIdentity,
    reason: &'static str,
) -> Result<(), AppError> {
    state.db.fail_response_archive_spool(identity, reason).await
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

    fn identity() -> ArchiveSpoolIdentity {
        ArchiveSpoolIdentity {
            request_id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            reservation_id: uuid::Uuid::nil(),
        }
    }

    #[test]
    fn buffered_archive_replays_its_body_but_new_captures_get_fresh_nonces() {
        let body = Bytes::from_static(b"immutable private response");
        let first = BufferedArchive::new(
            identity(),
            super::super::BufferedArchivePurpose::Response,
            &body,
            b"archive-spool-test-pepper",
        )
        .unwrap();
        let second = BufferedArchive::new(
            identity(),
            super::super::BufferedArchivePurpose::Response,
            &body,
            b"archive-spool-test-pepper",
        )
        .unwrap();

        assert_eq!(first.seal(0).unwrap(), first.seal(0).unwrap());
        assert_ne!(first.seal(0).unwrap(), second.seal(0).unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn acknowledgement_deadline_fails_closed_without_cancelling_the_operation() {
        let (release, released) = tokio::sync::oneshot::channel();
        let (settled, settlement) = tokio::sync::oneshot::channel();
        let started = tokio::time::Instant::now();
        let result = super::super::await_owned(
            super::super::ACK_TIMEOUT,
            async move {
                released.await.unwrap();
                settled.send(()).unwrap();
                Ok(())
            },
            "response_spool_ack_test",
        )
        .await;

        assert!(result.is_none());
        assert_eq!(started.elapsed(), super::super::ACK_TIMEOUT);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), settlement)
            .await
            .expect("timed-out acknowledgement must settle in its owned task")
            .unwrap();
    }
}
