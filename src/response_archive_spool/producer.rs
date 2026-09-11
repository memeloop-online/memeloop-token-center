use std::future::Future;

use bytes::Bytes;

use crate::{AppState, db::ArchiveSpoolIdentity, error::AppError};

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
