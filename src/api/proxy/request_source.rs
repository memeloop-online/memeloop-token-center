use std::{io, pin::Pin, sync::Arc};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::{
    error::AppError,
    gateway_body::{memory::ProxyMemoryReservation, request_spool::RequestSpool},
};

struct SourceReader {
    runtime: tokio::runtime::Handle,
    stream: Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>,
    pending: Bytes,
    cancelled: tokio::sync::oneshot::Receiver<()>,
}

impl io::Read for SourceReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.cancelled.try_recv() != Err(tokio::sync::oneshot::error::TryRecvError::Empty) {
            return Err(io::Error::other("request source cancelled"));
        }
        while self.pending.is_empty() {
            let next = self.runtime.block_on(async {
                tokio::select! {
                    _ = &mut self.cancelled => Err(io::Error::other("request source cancelled")),
                    next = self.stream.next() => Ok(next),
                }
            })?;
            match next {
                Some(bytes) => self.pending = bytes?,
                None => return Ok(0),
            }
        }
        let count = output.len().min(self.pending.len());
        output[..count].copy_from_slice(&self.pending[..count]);
        self.pending = self.pending.slice(count..);
        Ok(count)
    }
}

pub(super) async fn parse(
    source: Arc<RequestSpool>,
    memory: Arc<ProxyMemoryReservation>,
) -> Result<Value, AppError> {
    let mut working = None;
    source.reclaim_for(|| {
        working = memory.try_reserve_archive_output(64 * 1024);
        working.is_some()
    });
    let working = working.ok_or_else(|| AppError::capacity_error("proxy_memory_json"))?;
    if !source.reclaim_for(|| memory.try_reserve_json_shape(source.len(), source.json_nodes())) {
        return Err(AppError::capacity_error("proxy_memory_json"));
    }
    let download = source
        .open_stream()
        .await
        .map_err(|_| AppError::capacity_error("request_spool_read_failed"))?;
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel();
    let reader = SourceReader {
        runtime: tokio::runtime::Handle::current(),
        stream: download.stream,
        pending: Bytes::new(),
        cancelled,
    };
    let parsed = tokio::task::spawn_blocking(move || {
        let _source = source;
        let _memory = memory;
        let _working = working;
        super::super::sse::parse_unique_json_reader(io::BufReader::with_capacity(64 * 1024, reader))
    })
    .await
    .map_err(|_| AppError::internal("request_source_parser_join"))?;
    drop(cancel_on_drop);
    parsed.map_err(|error| {
        if error.is_io() {
            AppError::capacity_error("request_spool_read_failed")
        } else {
            AppError::BadRequest("request body must be valid JSON".into())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_body::{memory::ProxyMemoryBudget, request_spool::RequestSpoolAdmission};

    async fn captured(body: Bytes) -> (crate::archive::ArchiveStore, Arc<RequestSpool>) {
        let archive = crate::archive::ArchiveStore::from_config(&crate::config::Config::for_test(
            "sqlite::memory:".into(),
        ))
        .await
        .unwrap();
        let admission =
            RequestSpoolAdmission::new(std::env::temp_dir()).with_archive(archive.clone());
        let source = admission
            .capture_with_memory(
                axum::body::Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                None,
                0,
                None,
            )
            .await
            .unwrap();
        assert!(source.upload_outcome().await);
        source.release_retained();
        (archive, Arc::new(source))
    }

    #[tokio::test]
    async fn local_spill_parses_without_an_archive_upload() {
        let value = serde_json::json!({"model": "test", "input": "x".repeat(96 * 1024)});
        let body = Bytes::from(serde_json::to_vec(&value).unwrap());
        let admission = RequestSpoolAdmission::new(std::env::temp_dir());
        let exhausted = ProxyMemoryBudget::new(0);
        let source = admission
            .capture_with_memory(
                axum::body::Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                Some(&exhausted.reservation()),
                0,
                None,
            )
            .await
            .unwrap();
        let memory = ProxyMemoryBudget::new(1024 * 1024).reservation();
        assert_eq!(parse(Arc::new(source), memory).await.unwrap(), value);
    }

    #[tokio::test]
    async fn durable_body_parses_without_a_second_full_raw_allocation() {
        let content = "x".repeat(256 * 1024);
        let value = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": content}],
            "tools": [{"type": "function", "function": {"name": "read"}}]
        });
        let body = Bytes::from(serde_json::to_vec(&value).unwrap());
        let (_, source) = captured(body.clone()).await;
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        assert!(!budget.reservation().try_grow(body.len(), 6));
        let memory = budget.reservation();
        assert_eq!(parse(source.clone(), memory.clone()).await.unwrap(), value);
        assert!(budget.snapshot().0 <= 1024 * 1024);
        drop(memory);
        assert_eq!(budget.snapshot().0, 0);
        assert_eq!(source.read_all().await.unwrap(), body);
    }

    #[tokio::test]
    async fn stored_body_parse_capacity_is_not_reported_as_ingress_or_store_failure() {
        let (archive, source) = captured(Bytes::from_static(br#"{"model":"test"}"#)).await;
        let memory = ProxyMemoryBudget::new(0).reservation();
        assert!(matches!(
            parse(source.clone(), memory).await,
            Err(AppError::Capacity {
                code: "proxy_memory_json",
                ..
            })
        ));
        assert_eq!(
            archive.get(source.object_locator().unwrap()).await.unwrap(),
            br#"{"model":"test"}"#[..]
        );
    }

    #[tokio::test]
    async fn streamed_source_keeps_duplicate_key_and_hash_checks() {
        for body in [
            br#"{"key":1,"key":2}"#.as_slice(),
            br#"{"key":1}{}"#,
            b"{",
            b"",
        ] {
            let (_, source) = captured(Bytes::copy_from_slice(body)).await;
            assert!(matches!(
                parse(source, ProxyMemoryBudget::new(1024 * 1024).reservation()).await,
                Err(AppError::BadRequest(_))
            ));
        }
        let (archive, source) = captured(Bytes::from_static(br#"{"key":1}"#)).await;
        archive
            .put(
                source.object_locator().unwrap(),
                Bytes::from_static(br#"{"key":2}"#),
            )
            .await
            .unwrap();
        assert!(matches!(
            parse(source, ProxyMemoryBudget::new(1024 * 1024).reservation()).await,
            Err(AppError::Capacity {
                code: "request_spool_read_failed",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn dropped_request_interrupts_a_waiting_source_reader() {
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let reader = SourceReader {
            runtime: tokio::runtime::Handle::current(),
            stream: Box::pin(futures_util::stream::pending()),
            pending: Bytes::new(),
            cancelled,
        };
        let task = tokio::task::spawn_blocking(move || {
            super::super::super::sse::parse_unique_json_reader(reader)
        });
        drop(cancel);
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().is_io());
    }
}
