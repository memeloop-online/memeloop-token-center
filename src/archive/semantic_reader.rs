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
