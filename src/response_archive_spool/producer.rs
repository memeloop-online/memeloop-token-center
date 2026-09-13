use std::future::Future;

use bytes::Bytes;

use crate::{AppState, db::ArchiveSpoolIdentity, error::AppError};

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

impl ResponseArchiveProducer {
    pub(crate) async fn begin(state: &AppState, identity: ArchiveSpoolIdentity) -> Option<Self> {
        match bounded_ack(Self::begin_inner(state, identity)).await {
            Some(producer) => Some(producer),
            _ => {
                tracing::warn!(request_id = %identity.request_id, stage = "response_spool_admission", "proxy archive gap");
                None
            }
        }
    }

    async fn begin_inner(
        state: &AppState,
        identity: ArchiveSpoolIdentity,
    ) -> Result<Self, AppError> {
        if !state.db.begin_response_archive_spool(identity).await? {
            return Err(AppError::Internal);
        }
        Ok(Self {
            state: state.clone(),
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
        Self::begin_inner(state, identity).await
    }

    pub(crate) async fn append(&mut self, chunks: Vec<Bytes>) -> bool {
        #[cfg(test)]
        if take_append_failure_for_test(&self.state) {
            return false;
        }
        let _memory = self.state.metrics.memory_usage(
            crate::metrics::MemoryComponent::StreamCapture,
            super::CHUNK_BYTES * 5,
        );
        bounded_ack(self.append_inner(chunks)).await.is_some()
    }

    async fn append_inner(&mut self, chunks: Vec<Bytes>) -> Result<(), AppError> {
        let mut buffered = Vec::with_capacity(super::CHUNK_BYTES);
        for chunk in chunks {
            let mut remaining = chunk.as_ref();
            while !remaining.is_empty() {
                let take = remaining.len().min(super::CHUNK_BYTES - buffered.len());
                buffered.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if buffered.len() == super::CHUNK_BYTES {
                    self.append_chunk(&buffered).await?;
                    buffered.clear();
                }
            }
        }
        if !buffered.is_empty() {
            self.append_chunk(&buffered).await?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn append_for_test(&mut self, chunks: Vec<Bytes>) -> Result<(), AppError> {
        self.append_inner(chunks).await
    }

    async fn append_chunk(&mut self, bytes: &[u8]) -> Result<(), AppError> {
        let ciphertext = super::cipher::seal(
            self.identity,
            self.seq,
            bytes,
            self.state.config.key_pepper.as_bytes(),
        )?;
        let byte_count = i64::try_from(bytes.len()).map_err(|_| AppError::Internal)?;
        if !self
            .state
            .db
            .append_response_archive_spool(self.identity, self.seq, byte_count, &ciphertext)
            .await?
        {
            return Err(AppError::Internal);
        }
        self.seq += 1;
        self.bytes += byte_count;
        Ok(())
    }

    pub(crate) async fn seal(self) -> bool {
        bounded_ack(self.seal_inner()).await.is_some()
    }

    async fn seal_inner(self) -> Result<(), AppError> {
        if !self
            .state
            .db
            .seal_response_archive_spool(self.identity, self.seq, self.bytes)
            .await?
        {
            return Err(AppError::Internal);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn seal_for_test(self) -> Result<(), AppError> {
        self.seal_inner().await
    }
}

async fn bounded_ack<T>(future: impl Future<Output = Result<T, AppError>>) -> Option<T> {
    tokio::time::timeout(super::ACK_TIMEOUT, future)
        .await
        .ok()?
        .ok()
}

pub(crate) async fn mark_gap(
    state: &AppState,
    identity: ArchiveSpoolIdentity,
    reason: &'static str,
) {
    // Lost database ACKs are not retried by resending upstream or recharging.
    // Stale unsealed captures are independently fenced and expired by worker.
    let result = bounded_ack(mark_gap_inner(state, identity, reason)).await;
    if result.is_none() {
        tracing::warn!(request_id = %identity.request_id, stage = "response_spool_gap_ack", "proxy archive gap");
    }
}

async fn mark_gap_inner(
    state: &AppState,
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
    mark_gap_inner(state, identity, reason).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn acknowledgement_deadline_fails_closed_without_completion() {
        let started = tokio::time::Instant::now();
        let result = bounded_ack(std::future::pending::<Result<(), AppError>>()).await;

        assert!(result.is_none());
        assert_eq!(started.elapsed(), super::super::ACK_TIMEOUT);
    }
}
