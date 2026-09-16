//! Lossless v1 text objects. The locator, not payload sniffing, selects the
//! decoder. Each independently bounded frame and the complete plaintext have
//! BLAKE3 checksums. Range reads scan/verify the object, never expose wire bytes.
use std::{ops::Range, pin::Pin};

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt, stream};
use object_store::{GetOptions, GetRange};

use super::{ArchiveDownload, ArchiveStore, path::archive_path};
use crate::error::AppError;

pub(super) const SUFFIX: &str = ".mtcz1";
pub(super) const BLOCK: usize = 64 * 1024;
const MAX_ENCODED_BLOCK: usize = 128 * 1024;
pub(super) const MAX_PLAIN: u64 = 64 * 1024 * 1024;
pub(super) const MAGIC: &[u8; 8] = b"MTCZSTD1";
const END: &[u8; 8] = b"MTCZEND1";
const FOOTER: u64 = 48;

fn invalid() -> std::io::Error {
    std::io::Error::other("archive compressed object integrity failure")
}

fn storage(_: impl std::fmt::Debug) -> AppError {
    AppError::Storage("archive compressed object integrity failure".into())
}

pub(super) fn frame(plain: &[u8]) -> Result<Bytes, AppError> {
    if plain.is_empty() || plain.len() > BLOCK {
        return Err(storage(()));
    }
    let compressed = zstd::bulk::compress(plain, 1).map_err(storage)?;
    if compressed.len() > MAX_ENCODED_BLOCK {
        return Err(storage(()));
    }
    let mut output = Vec::with_capacity(40 + compressed.len());
    output.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    output.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
    output.extend_from_slice(blake3::hash(plain).as_bytes());
    output.extend_from_slice(&compressed);
    Ok(Bytes::from(output))
}

pub(super) fn footer(size: u64, hash: &blake3::Hasher) -> Bytes {
    let mut output = Vec::with_capacity(FOOTER as usize);
    output.extend_from_slice(END);
    output.extend_from_slice(&size.to_le_bytes());
    output.extend_from_slice(hash.finalize().as_bytes());
    Bytes::from(output)
}

struct Metadata {
    physical: u64,
    logical: u64,
    digest: [u8; 32],
    etag: Option<String>,
    version: Option<String>,
}

impl ArchiveStore {
    async fn compressed_metadata(&self, location: &str) -> Result<Metadata, AppError> {
        let result = self
            .inner
            .get_opts(
                &archive_path(location)?,
                GetOptions {
                    range: Some(GetRange::Suffix(FOOTER)),
                    ..GetOptions::default()
                },
            )
            .await?;
        let physical = result.meta.size;
        let etag = result.meta.e_tag.clone();
        let version = result.meta.version.clone();
        if !(8 + FOOTER..=MAX_PLAIN * 3).contains(&physical)
            || result.range != (physical - FOOTER..physical)
        {
            return Err(storage(()));
        }
        let bytes = result.bytes().await?;
        if bytes.len() != FOOTER as usize || &bytes[..8] != END {
            return Err(storage(()));
        }
        let logical = u64::from_le_bytes(bytes[8..16].try_into().map_err(storage)?);
        if logical > MAX_PLAIN {
            return Err(storage(()));
        }
        Ok(Metadata {
            physical,
            logical,
            digest: bytes[16..48].try_into().map_err(storage)?,
            etag,
            version,
        })
    }

    pub(super) async fn compressed_size(&self, location: &str) -> Result<u64, AppError> {
        Ok(self.compressed_metadata(location).await?.logical)
    }

    pub(super) async fn compressed_stream(
        &self,
        location: &str,
        range: Option<Range<u64>>,
    ) -> Result<ArchiveDownload, AppError> {
        let metadata = self.compressed_metadata(location).await?;
        let requested = range.unwrap_or(0..metadata.logical);
        super::download::validate_download_range(metadata.logical, Some(&requested), &requested)?;
        let result = self
            .inner
            .get_opts(
                &archive_path(location)?,
                GetOptions {
                    if_match: metadata.etag.clone(),
                    version: metadata.version.clone(),
                    ..GetOptions::default()
                },
            )
            .await?;
        if result.meta.size != metadata.physical || result.range != (0..metadata.physical) {
            return Err(storage(()));
        }
        let state = Decoder {
            source: result.into_stream(),
            carry: Bytes::new(),
            metadata,
            position: 0,
            physical: 0,
            hash: blake3::Hasher::new(),
            range: requested.clone(),
            pending: None,
            done: false,
        };
        let size = state.metadata.logical;
        let decoded = stream::try_unfold(state, |mut state| async move {
            if state.done {
                return Ok(None);
            }
            if state.physical == 0 && state.read(8).await?.as_ref() != MAGIC {
                return Err(invalid());
            }
            loop {
                if state.physical == state.metadata.physical - FOOTER {
                    let trailer = state.read(FOOTER as usize).await?;
                    if trailer != footer(state.position, &state.hash)
                        || state.position != state.metadata.logical
                        || state.hash.finalize().as_bytes() != &state.metadata.digest
                        || !state.carry.is_empty()
                    {
                        return Err(invalid());
                    }
                    while let Some(extra) = state.source.next().await {
                        if !extra.map_err(|_| invalid())?.is_empty() {
                            return Err(invalid());
                        }
                    }
                    state.done = true;
                    return Ok(state.pending.take().map(|bytes| (bytes, state)));
                }
                if state.physical + 40 > state.metadata.physical - FOOTER {
                    return Err(invalid());
                }
                let header = state.read(40).await?;
                let plain_len =
                    u32::from_le_bytes(header[..4].try_into().map_err(|_| invalid())?) as usize;
                let encoded_len =
                    u32::from_le_bytes(header[4..8].try_into().map_err(|_| invalid())?) as usize;
                if !(1..=BLOCK).contains(&plain_len)
                    || !(1..=MAX_ENCODED_BLOCK).contains(&encoded_len)
                    || state.physical + encoded_len as u64 > state.metadata.physical - FOOTER
                    || state.position + plain_len as u64 > state.metadata.logical
                {
                    return Err(invalid());
                }
                let encoded = state.read(encoded_len).await?;
                let plain = zstd::bulk::decompress(&encoded, plain_len).map_err(|_| invalid())?;
                if plain.len() != plain_len || blake3::hash(&plain).as_bytes() != &header[8..40] {
                    return Err(invalid());
                }
                state.hash.update(&plain);
                let start = state.range.start.max(state.position);
                let end = state.range.end.min(state.position + plain_len as u64);
                let offset = state.position;
                state.position += plain_len as u64;
                if start < end {
                    let selected = Bytes::from(plain)
                        .slice((start - offset) as usize..(end - offset) as usize);
                    // Hold the final selected frame until the complete object's
                    // length/checksum/EOF have been verified (also for ranges).
                    if let Some(previous) = state.pending.replace(selected) {
                        return Ok(Some((previous, state)));
                    }
                }
            }
        });
        Ok(ArchiveDownload {
            object_size: size,
            range: requested,
            stream: Box::pin(decoded),
        })
    }
}

struct Decoder {
    source: Pin<Box<dyn Stream<Item = object_store::Result<Bytes>> + Send>>,
    carry: Bytes,
    metadata: Metadata,
    position: u64,
    physical: u64,
    hash: blake3::Hasher,
    range: Range<u64>,
    pending: Option<Bytes>,
    done: bool,
}

impl Decoder {
    async fn read(&mut self, size: usize) -> Result<Bytes, std::io::Error> {
        let mut output = BytesMut::with_capacity(size);
        while output.len() < size {
            if self.carry.is_empty() {
                self.carry = self
                    .source
                    .next()
                    .await
                    .ok_or_else(invalid)?
                    .map_err(|_| invalid())?;
                continue;
            }
            let length = (size - output.len()).min(self.carry.len());
            output.extend_from_slice(&self.carry.split_to(length));
        }
        self.physical += size as u64;
        Ok(output.freeze())
    }
}
