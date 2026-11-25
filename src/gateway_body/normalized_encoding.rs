//! Private normalized replay: preserve the original archive and transfer only
//! owned JSON credit, spilling directly to a typed temporary object if needed.
use super::{
    carrier::{BodyCarrier, CarrierCapture},
    memory::{ProxyMemoryBudget, ProxyMemoryReservation},
};
use crate::{
    archive::{ArchiveStore, scratch_scope::ScratchScope},
    archive_staging::ArchiveStagingKey,
    db::Database,
    error::AppError,
};
use bytes::Bytes;
use serde_json::Value;
use std::{io, sync::Arc};

pub(crate) async fn encode(
    value: Value,
    previous: Arc<ProxyMemoryReservation>,
    memory: Arc<ProxyMemoryBudget>,
    archive: ArchiveStore,
    database: Database,
    staging: ArchiveStagingKey,
    upload: Arc<crate::api::proxy::persistence::Persistence>,
) -> Result<Arc<BodyCarrier>, AppError> {
    let capture = CarrierCapture::new(memory, Some(archive.clone()), staging);
    let (value, capture) = match BodyCarrier::encode_normalized_prepaid(value, &previous, capture)?
    {
        Ok(source) => {
            previous.release_unretained_json();
            return Ok(source);
        }
        Err(overflow) => overflow,
    };
    // Borrow only already-owned JSON workspace before waiting for upload.
    // Holding an upload envelope must never lead to another source acquisition.
    let mut size = 64 * 1024;
    let workspace = loop {
        if let Some(guard) = previous.borrow_route_workspace(size) {
            break guard;
        }
        if size == 1 {
            return Err(AppError::internal("normalized_workspace_not_owned"));
        }
        size = (size / 2).max(1);
    };
    let upload_memory = upload
        .multipart_workspace(crate::api::proxy::persistence::WorkspaceRole::Normalized)
        .await?;
    let scope = Arc::new(ScratchScope::new(archive, database, staging)?);
    let (archive, selected) = scope.begin_body_write().await?;
    let mut writer = archive
        .start_writer(&format!("{}/normalized.json", selected.canonical_prefix()))
        .await?;
    writer.retain_workspace(upload_memory);
    let runtime = tokio::runtime::Handle::current();
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
    let writer_scope = scope.clone();
    let staged = tokio::task::spawn_blocking(move || {
        let _scope = writer_scope;
        struct Writer {
            runtime: tokio::runtime::Handle,
            writer: crate::archive::ArchiveWriter,
            size: usize,
            cancelled: tokio::sync::oneshot::Receiver<()>,
        }
        impl io::Write for Writer {
            fn write(&mut self, input: &[u8]) -> io::Result<usize> {
                if self.cancelled.try_recv()
                    != Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                {
                    return Err(io::Error::other("normalized encoding cancelled"));
                }
                let count = input.len().min(self.size);
                let runtime = self.runtime.clone();
                runtime
                    .block_on(cancel_upload(
                        &mut self.cancelled,
                        self.writer.write(Bytes::copy_from_slice(&input[..count])),
                    ))
                    .map_err(|error| io::Error::other(error.to_string()))?;
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = Writer {
            runtime,
            writer,
            size,
            cancelled,
        };
        let result = serde_json::to_writer(&mut output, &value);
        drop(value);
        drop(workspace);
        result.map_err(|_| AppError::storage("normalized_encoding_write_failed"))?;
        let Writer {
            runtime,
            writer,
            mut cancelled,
            ..
        } = output;
        runtime.block_on(cancel_upload(&mut cancelled, writer.finish_staged()))
    })
    .await
    .map_err(|_| AppError::internal("normalized_encoding_join"))??;
    drop(cancel_on_drop);
    scope.confirm_body_write()?;
    let source = BodyCarrier::from_owned_staged(staged, capture, scope)?;
    previous.release_unretained_json();
    Ok(source)
}

async fn cancel_upload<T>(
    cancelled: &mut tokio::sync::oneshot::Receiver<()>,
    operation: impl std::future::Future<Output = Result<T, AppError>>,
) -> Result<T, AppError> {
    tokio::select! {
        _ = cancelled => Err(AppError::storage("normalized_encoding_cancelled")),
        result = operation => result,
    }
}

#[cfg(test)]
mod cancellation_contracts {
    use super::*;
    #[tokio::test]
    async fn pending_write_and_finish_cancel_and_return_actual_upload_envelope() {
        for phase in ["write", "finish"] {
            let upload = Arc::new(crate::api::proxy::persistence::Persistence::default());
            let bytes = crate::archive::ARCHIVE_MULTIPART_PART_BYTES + 5 * 64 * 1024;
            let envelope = upload
                .upload_workspace
                .temporary_wait(bytes, super::super::memory::CAPTURE_MEMORY_WEIGHT)
                .await
                .unwrap();
            assert!(upload.upload_workspace.snapshot().0 >= bytes);
            let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
            let (entered, started) = tokio::sync::oneshot::channel();
            let runtime = tokio::runtime::Handle::current();
            let task = tokio::task::spawn_blocking(move || {
                let _envelope = envelope;
                runtime.block_on(cancel_upload(&mut cancelled, async move {
                    entered.send(()).unwrap();
                    std::future::pending::<Result<(), AppError>>().await
                }))
            });
            started.await.unwrap();
            drop(cancel);
            let result = task.await.unwrap();
            assert!(
                result.is_err(),
                "pending {phase} must exit after caller cancellation"
            );
            assert_eq!(
                upload.upload_workspace.snapshot().0,
                0,
                "{phase} must return the full multipart envelope"
            );
        }
    }
}
