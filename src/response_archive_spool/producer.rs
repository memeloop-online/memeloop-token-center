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
        match tokio::time::timeout(
            super::ACK_TIMEOUT,
            state.db.begin_response_archive_spool(identity),
        )
        .await
        {
            Ok(Ok(true)) => Some(Self {
                state: state.clone(),
                identity,
                seq: 0,
                bytes: 0,
            }),
            _ => {
                tracing::warn!(request_id = %identity.request_id, stage = "response_spool_admission", "proxy archive gap");
                None
            }
        }
    }

    pub(crate) async fn append(&mut self, chunks: Vec<Bytes>) -> bool {
        let _memory = self.state.metrics.memory_usage(
            crate::metrics::MemoryComponent::StreamCapture,
            super::CHUNK_BYTES * 5,
        );
        matches!(
            tokio::time::timeout(super::ACK_TIMEOUT, self.append_inner(chunks)).await,
            Ok(Ok(()))
        )
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
        matches!(
            tokio::time::timeout(
                super::ACK_TIMEOUT,
                self.state
                    .db
                    .seal_response_archive_spool(self.identity, self.seq, self.bytes,),
            )
            .await,
            Ok(Ok(true))
        )
    }
}

pub(crate) async fn mark_gap(
    state: &AppState,
    identity: ArchiveSpoolIdentity,
    reason: &'static str,
) {
    // Lost database ACKs are not retried by resending upstream or recharging.
    // Stale unsealed captures are independently fenced and expired by worker.
    let result = tokio::time::timeout(
        super::ACK_TIMEOUT,
        state.db.fail_response_archive_spool(identity, reason),
    )
    .await;
    if !matches!(result, Ok(Ok(()))) {
        tracing::warn!(request_id = %identity.request_id, stage = "response_spool_gap_ack", "proxy archive gap");
    }
}
