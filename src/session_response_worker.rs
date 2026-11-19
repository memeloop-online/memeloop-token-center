use crate::{AppState, db::SessionResponseTask, error::AppError, model::RequestArchiveState};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use std::{
    io::{self, BufReader, Cursor, Read},
    pin::Pin,
};

pub(crate) async fn process_one(state: &AppState) -> Result<(), AppError> {
    let Some(task) = state
        .db
        .claim_session_response(uuid::Uuid::now_v7())
        .await?
    else {
        return Ok(());
    };
    let result = project(state, &task).await;
    match result {
        Ok(text) => state.db.finish_session_response(&task, text, None).await,
        Err(reason) => {
            state
                .db
                .finish_session_response(&task, None, Some(reason))
                .await
        }
    }
}

async fn project(
    state: &AppState,
    task: &SessionResponseTask,
) -> Result<Option<String>, &'static str> {
    let refs = state
        .db
        .request_archive_refs(task.key_id, task.request_id)
        .await
        .map_err(|_| "archive_unavailable")?;
    if matches!(
        refs.response_archive_state,
        RequestArchiveState::Pending
            | RequestArchiveState::Capturing
            | RequestArchiveState::Uploading
    ) || (refs.view.completed_at.is_none()
        && refs.view.source_completed_at.is_none()
        && refs.view.status_code.is_none())
    {
        return Err("archive_pending");
    }
    let Some(location) = refs.response_object else {
        if let Some(json) = refs.response_json {
            let bytes = serde_json::to_vec(&json).map_err(|_| "archive_invalid_json")?;
            return tokio::task::spawn_blocking(move || {
                crate::session_response::extract(Cursor::new(bytes))
            })
            .await
            .map_err(|_| "projection_failed")?;
        }
        return Err("archive_unavailable");
    };
    if let Some(json) = location.strip_prefix("inline-json:") {
        let bytes = json.as_bytes().to_vec();
        return tokio::task::spawn_blocking(move || {
            crate::session_response::extract(Cursor::new(bytes))
        })
        .await
        .map_err(|_| "projection_failed")?;
    }
    if !matches!(refs.response_archive_state, RequestArchiveState::Bound) {
        return Err("archive_unavailable");
    }
    let source = state
        .archive
        .open_stream(&location, None)
        .await
        .map_err(|_| "archive_unavailable")?;
    extract_stream(source.stream).await
}

/// The verified object stream is consumed directly; no full replay copy or
/// temporary file survives while a remote object is being read.
async fn extract_stream(
    stream: Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>,
) -> Result<Option<String>, &'static str> {
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel();
    let reader = ArchiveReader {
        runtime: tokio::runtime::Handle::current(),
        stream,
        pending: Bytes::new(),
        cancelled,
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::session_response::extract(BufReader::with_capacity(64 * 1024, reader))
    })
    .await
    .map_err(|_| "projection_failed")?;
    drop(cancel_on_drop);
    result
}

struct ArchiveReader {
    runtime: tokio::runtime::Handle,
    stream: Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>,
    pending: Bytes,
    cancelled: tokio::sync::oneshot::Receiver<()>,
}

impl Read for ArchiveReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.cancelled.try_recv() != Err(tokio::sync::oneshot::error::TryRecvError::Empty) {
            return Err(io::Error::other("session response projection cancelled"));
        }
        while self.pending.is_empty() {
            let next = self.runtime.block_on(async {
                tokio::select! {
                    _ = &mut self.cancelled => Err(io::Error::other("session response projection cancelled")),
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

#[cfg(test)]
mod stream_reader_contracts {
    use super::*;
    use futures_util::stream;

    #[tokio::test]
    async fn archived_response_is_parsed_across_chunks_without_local_storage() {
        let wire = br#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"saved answer"}]}]}"#;
        let chunks: Vec<_> = wire
            .chunks(3)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        assert_eq!(
            extract_stream(Box::pin(stream::iter(chunks)))
                .await
                .unwrap()
                .as_deref(),
            Some("saved answer")
        );
    }

    #[tokio::test]
    async fn object_read_failure_is_not_reported_as_a_completed_answer() {
        let chunks = stream::iter(vec![
            Ok(Bytes::from_static(b"{\"output\":")),
            Err(io::Error::other("object disconnected")),
        ]);
        assert!(extract_stream(Box::pin(chunks)).await.is_err());
    }

    #[tokio::test]
    async fn cancelling_projection_releases_the_waiting_object_reader() {
        let (dropped, receiver) = tokio::sync::oneshot::channel();
        let source = stream::unfold(Some(dropped), |owner| async move {
            let _owner = owner;
            futures_util::future::pending::<
                Option<(io::Result<Bytes>, Option<tokio::sync::oneshot::Sender<()>>)>,
            >()
            .await
        });
        let mut projection = Box::pin(extract_stream(Box::pin(source)));
        assert!(matches!(
            futures_util::poll!(&mut projection),
            std::task::Poll::Pending
        ));
        drop(projection);
        assert!(receiver.await.is_err());
    }
}
