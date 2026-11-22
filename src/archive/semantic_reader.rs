//! Bounded typed projection of the existing verified canonical atom stream.
use super::{ArchiveDownload, conversation_stream::Chunks};
use crate::error::AppError;
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Serialize;
use std::io::{BufRead, BufReader};

mod scanner;
const PAGE: usize = 32;
pub(crate) const CHUNK: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) enum Selection {
    Parts {
        start: usize,
    },
    Content {
        part: usize,
        field: String,
        offset: u64,
    },
    Media {
        part: usize,
    },
}
#[derive(Default, Serialize)]
pub(crate) struct Part {
    pub part_index: usize,
    #[serde(skip)]
    pub ordinal: usize,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completeness: Option<String>,
    pub fields: Vec<Field>,
    pub metadata_deferred: bool,
    #[serde(skip)]
    pub media_type: Option<String>,
    #[serde(skip)]
    pub byte_length: Option<u64>,
    #[serde(skip)]
    pub locator: Option<String>,
    #[serde(skip)]
    pub digest: Option<String>,
}
#[derive(Serialize)]
pub(crate) struct Field {
    pub name: String,
    pub format: String,
}
pub(crate) struct Projection {
    pub role: Option<String>,
    pub parts: Vec<Part>,
    pub more: bool,
    pub text: String,
    pub complete: bool,
    pub format: Option<String>,
    pub stream_end: Option<u64>,
}

pub(crate) async fn project(
    download: ArchiveDownload,
    selection: Selection,
) -> Result<Projection, AppError> {
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    let parsed = tokio::task::spawn_blocking(move || {
        scanner::scan(
            BufReader::with_capacity(
                8192,
                Chunks {
                    receiver,
                    current: Bytes::new(),
                },
            ),
            selection,
        )
    });
    let mut source = download.stream;
    let pumped = async {
        while let Some(chunk) = source.next().await {
            let chunk = chunk.map_err(|_| AppError::storage("semantic_atom_stream_failed"))?;
            for piece in chunk.chunks(CHUNK) {
                if sender.send(Bytes::copy_from_slice(piece)).await.is_err() {
                    return Err(AppError::storage("semantic_atom_invalid"));
                }
            }
        }
        Ok::<_, AppError>(())
    }
    .await;
    drop(sender);
    let projection = parsed
        .await
        .map_err(|_| AppError::storage("semantic_atom_parse_failed"))?
        .map_err(|_| AppError::storage("semantic_atom_invalid"))?;
    pumped?;
    Ok(projection)
}

#[cfg(test)]
mod tests;

pub(crate) struct ContentPiece {
    pub text: String,
    pub offset: u64,
    pub complete: bool,
}

/// One request scans the atom once. Backpressure bounds both the source and
/// decoded output queues. Dropping the HTTP body closes the output receiver,
/// which stops the pump and releases the verified object-store stream.
pub(crate) fn content_stream(
    download: ArchiveDownload,
    selection: Selection,
    expected_index: Option<usize>,
) -> tokio_stream::wrappers::ReceiverStream<std::io::Result<ContentPiece>> {
    let (output, receiver) = tokio::sync::mpsc::channel(2);
    tokio::spawn(async move {
        let (sender, input) = tokio::sync::mpsc::channel(2);
        let frames = output.clone();
        let offset = match &selection {
            Selection::Content { offset, .. } => *offset,
            _ => 0,
        };
        let parsed = tokio::task::spawn_blocking(move || {
            scanner::scan_with_output(
                BufReader::with_capacity(
                    8192,
                    Chunks {
                        receiver: input,
                        current: Bytes::new(),
                    },
                ),
                selection,
                Some(frames),
            )
        });
        let mut source = download.stream;
        let pumped = async {
            loop {
                let next = tokio::select! { _ = output.closed() => return Err(std::io::Error::other("reader closed")), next = source.next() => next };
                let Some(chunk) = next else {break;};
                for piece in chunk?.chunks(CHUNK) {
                    tokio::select! { _ = output.closed() => return Err(std::io::Error::other("reader closed")), result = sender.send(Bytes::copy_from_slice(piece)) => result.map_err(|_| std::io::Error::other("invalid semantic atom"))? }
                }
            }
            Ok::<_,std::io::Error>(())
        }.await;
        drop(sender);
        let result = match parsed.await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::other("semantic parser failed")),
        };
        match pumped.and(result) {
            Ok(projection)
                if expected_index.is_none_or(|expected| {
                    projection
                        .parts
                        .first()
                        .is_some_and(|part| part.part_index == expected)
                }) =>
            {
                // Only this terminal frame follows verified source EOF. All
                // preceding frames explicitly remain incomplete.
                let end_offset = projection.stream_end.unwrap_or(offset);
                let _ = output
                    .send(Ok(ContentPiece {
                        text: projection.text,
                        offset: end_offset,
                        complete: true,
                    }))
                    .await;
            }
            Ok(_) => {
                let _ = output
                    .send(Err(std::io::Error::other(
                        "semantic part identity mismatch",
                    )))
                    .await;
            }
            Err(error) => {
                let _ = output.send(Err(error)).await;
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(receiver)
}
