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
    let upload_memory = upload
        .stream_memory
        .temporary_wait(
            crate::archive::ARCHIVE_MULTIPART_PART_BYTES + 5 * 64 * 1024,
            super::memory::CAPTURE_MEMORY_WEIGHT,
        )
        .await
        .ok_or_else(|| AppError::internal("normalized_upload_envelope"))?;
    let scope = Arc::new(ScratchScope::new(archive, database, staging)?);
    let (archive, selected) = scope.begin_body_write().await?;
    let writer = archive
        .start_writer(&format!("{}/normalized.json", selected.canonical_prefix()))
        .await?;
    let runtime = tokio::runtime::Handle::current();
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
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
    let writer_scope = scope.clone();
    let staged = tokio::task::spawn_blocking(move || {
        let _scope = writer_scope;
        let _upload_memory = upload_memory;
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
                runtime.block_on(async {
                    tokio::select! {
                        _ = &mut self.cancelled => Err(io::Error::other("normalized encoding cancelled")),
                        result = self.writer.write(Bytes::copy_from_slice(&input[..count])) => result.map_err(|error| io::Error::other(error.to_string())),
                    }
                })?;
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
        let Writer { runtime, writer, mut cancelled, .. } = output;
        runtime.block_on(async {
            tokio::select! {
                _ = &mut cancelled => Err(AppError::storage("normalized_encoding_cancelled")),
                result = writer.finish_staged() => result,
            }
        })
    })
    .await
    .map_err(|_| AppError::internal("normalized_encoding_join"))??;
    drop(cancel_on_drop);
    scope.confirm_body_write()?;
    let source = BodyCarrier::from_owned_staged(staged, capture, scope)?;
    previous.release_unretained_json();
    Ok(source)
}
