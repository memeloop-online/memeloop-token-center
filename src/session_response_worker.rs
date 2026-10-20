use crate::{AppState, db::SessionResponseTask, error::AppError, model::RequestArchiveState};
use futures_util::StreamExt;
use std::{
    io::{BufReader, Cursor, Seek},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;

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
        Ok(text) => {
            state
                .db
                .finish_session_response(&task, text.as_deref(), None)
                .await
        }
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
            if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
                return Err("response_archive_too_large");
            }
            return tokio::task::spawn_blocking(move || {
                crate::session_response::extract(Cursor::new(bytes))
            })
            .await
            .map_err(|_| "projection_failed")?;
        }
        return Err("archive_unavailable");
    };
    if let Some(json) = location.strip_prefix("inline-json:") {
        if json.len() as u64 > MAX_ARCHIVE_BYTES {
            return Err("response_archive_too_large");
        }
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
    // One serial worker lane, bounded download and disk-backed parsing keep
    // large archived tool streams out of HTTP and database result buffers.
    tokio::fs::create_dir_all(&state.config.responses_request_spool_path)
        .await
        .map_err(|_| "projection_storage_unavailable")?;
    let file = tempfile::tempfile_in(&state.config.responses_request_spool_path)
        .map_err(|_| "projection_storage_unavailable")?;
    let mut file = tokio::fs::File::from_std(file);
    let download = async {
        let mut source = state
            .archive
            .open_stream(&location, None)
            .await
            .map_err(|_| "archive_unavailable")?;
        if source.object_size > MAX_ARCHIVE_BYTES {
            return Err("response_archive_too_large");
        }
        let mut received = 0_u64;
        while let Some(bytes) = source.stream.next().await {
            let bytes = bytes.map_err(|_| "archive_read_failed")?;
            received += bytes.len() as u64;
            if received > MAX_ARCHIVE_BYTES {
                return Err("response_archive_too_large");
            }
            file.write_all(&bytes)
                .await
                .map_err(|_| "projection_storage_unavailable")?;
        }
        file.flush()
            .await
            .map_err(|_| "projection_storage_unavailable")?;
        Ok::<(), &'static str>(())
    };
    tokio::time::timeout(Duration::from_secs(60), download)
        .await
        .map_err(|_| "archive_read_timeout")??;
    let mut file = file.into_std().await;
    // Await the blocking parser rather than detaching it on timeout. The input
    // is a bounded regular file and its anonymous temporary storage is dropped.
    tokio::task::spawn_blocking(move || {
        file.rewind().map_err(|_| "archive_read_failed")?;
        crate::session_response::extract(BufReader::new(file))
    })
    .await
    .map_err(|_| "projection_failed")?
}
