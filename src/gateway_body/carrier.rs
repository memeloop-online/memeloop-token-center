//! Immutable application bodies. Only RAM allocations and typed object-store
//! objects may carry bytes; migration journals are deliberately outside this API.
use std::{fmt::Display, pin::Pin, sync::Arc};

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt, stream};
use tokio::sync::OwnedSemaphorePermit;

use super::memory::ProxyMemoryBudget;
use crate::{
    archive::{ArchiveStore, StagedArchiveObject},
    archive_staging::ArchiveStagingKey,
    error::AppError,
};

pub(crate) type CarrierStream = Pin<Box<dyn Stream<Item = Result<Bytes, AppError>> + Send>>;

pub(crate) struct CarrierCapture {
    memory: Arc<ProxyMemoryBudget>,
    archive: Option<ArchiveStore>,
    staging: ArchiveStagingKey,
}

impl CarrierCapture {
    pub(crate) fn new(
        memory: Arc<ProxyMemoryBudget>,
        archive: Option<ArchiveStore>,
        staging: ArchiveStagingKey,
    ) -> Self {
        Self {
            memory,
            archive,
            staging,
        }
    }
}

// Bytes::clone/slice retain this owner, so dropping the carrier cannot release
// an allocation while a parser or upstream transport still holds its bytes.
struct Allocation {
    bytes: Bytes,
    _lease: OwnedSemaphorePermit,
}
impl AsRef<[u8]> for Allocation {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

enum Segment {
    Ram(Bytes),
    Object(StagedArchiveObject),
}

pub(crate) struct BodyCarrier {
    memory: Arc<ProxyMemoryBudget>,
    archive: Option<ArchiveStore>,
    staging: ArchiveStagingKey,
    segments: Vec<Segment>,
    length: u64,
    digest: String,
}

impl BodyCarrier {
    /// The input is borrowed by this future: callers need not spawn or make it
    /// 'static. Bytes are never aggregated into a whole-body buffer.
    pub(crate) async fn capture<S, E>(
        input: S,
        capture: CarrierCapture,
    ) -> Result<Arc<Self>, AppError>
    where
        S: Stream<Item = Result<Bytes, E>> + Send,
        E: Display,
    {
        let staging = match &capture.archive {
            Some(archive) => archive.select_staging_key(capture.staging)?,
            None => capture.staging,
        };
        let mut source = Self {
            memory: capture.memory,
            archive: capture.archive,
            staging,
            segments: Vec::new(),
            length: 0,
            digest: String::new(),
        };
        let mut hash = blake3::Hasher::new();
        futures_util::pin_mut!(input);
        while let Some(chunk) = input.next().await {
            let chunk = chunk.map_err(|_| AppError::storage("carrier_input_failed"))?;
            if chunk.is_empty() {
                continue;
            }
            source.length = source
                .length
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| AppError::storage("carrier_length_overflow"))?;
            hash.update(&chunk);
            let mut remaining = chunk;
            while !remaining.is_empty() {
                // Bounded owned allocations prevent a small slice from pinning
                // an arbitrarily large input frame for the lifetime of a request.
                let part = remaining.split_to(remaining.len().min(64 * 1024));
                let lease = loop {
                    if let Some(lease) = source.memory.try_reserve_ingress_bytes(part.len()) {
                        break lease;
                    }
                    if source.offload_one().await? {
                        continue;
                    }
                    break source.memory.reserve_carrier_bytes(part.len()).await?;
                };
                let bytes = Bytes::from_owner(Allocation {
                    bytes: Bytes::copy_from_slice(&part),
                    _lease: lease,
                });
                source.segments.push(Segment::Ram(bytes));
            }
        }
        source.digest = hash.finalize().to_hex().to_string();
        Ok(Arc::new(source))
    }

    async fn offload_one(&mut self) -> Result<bool, AppError> {
        let Some(archive) = &self.archive else {
            return Ok(false);
        };
        let Some(index) = self
            .segments
            .iter()
            .position(|segment| matches!(segment, Segment::Ram(_)))
        else {
            return Ok(false);
        };
        let Segment::Ram(bytes) = &self.segments[index] else {
            unreachable!()
        };
        let locator = format!(
            "{}/carrier-{}",
            self.staging.canonical_prefix(),
            uuid::Uuid::now_v7()
        );
        let mut writer = archive.start_writer(&locator).await?;
        if let Err(error) = writer.write(bytes.clone()).await {
            let _ = writer.abort().await;
            return Err(error);
        }
        let staged = writer.finish_staged().await?;
        if staged.size_bytes != bytes.len() as u64
            || staged.blake3_digest != blake3::hash(bytes).to_hex().as_str()
        {
            return Err(AppError::storage("carrier_stage_identity_mismatch"));
        }
        // A complete object, not an UploadPart ACK, is now replayable. Only
        // replacing this segment releases the unique RAM allocation lease.
        self.segments[index] = Segment::Object(staged);
        Ok(true)
    }

    pub(crate) fn len(&self) -> u64 {
        self.length
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn sealed_staged_reference(&self) -> Option<&StagedArchiveObject> {
        match self.segments.as_slice() {
            [Segment::Object(reference)] => Some(reference),
            _ => None,
        }
    }

    pub(crate) fn open_stream(self: &Arc<Self>) -> CarrierStream {
        let source = self.clone();
        Box::pin(stream::unfold(
            (source, 0usize, None::<CarrierStream>),
            |(source, mut index, mut remote)| async move {
                loop {
                    if let Some(stream) = &mut remote {
                        if let Some(chunk) = stream.next().await {
                            return Some((chunk, (source, index, remote)));
                        }
                        remote = None;
                    }
                    let segment = source.segments.get(index)?;
                    index += 1;
                    match segment {
                        Segment::Ram(bytes) => {
                            return Some((Ok(bytes.clone()), (source, index, remote)));
                        }
                        Segment::Object(reference) => {
                            let Some(archive) = &source.archive else {
                                let end = source.segments.len();
                                return Some((
                                    Err(AppError::storage("carrier_backend_missing")),
                                    (source, end, None),
                                ));
                            };
                            match archive.open_stream(&reference.object_locator, None).await {
                                Ok(download) => {
                                    remote = Some(Box::pin(download.stream.map(|result| {
                                        result.map_err(|_| {
                                            AppError::storage("carrier_object_read_failed")
                                        })
                                    })))
                                }
                                Err(error) => return Some((Err(error), (source, index, None))),
                            }
                        }
                    }
                }
            },
        ))
    }

    pub(crate) async fn read_range(
        self: &Arc<Self>,
        offset: u64,
        length: usize,
    ) -> Result<Bytes, AppError> {
        let end = offset
            .checked_add(length as u64)
            .filter(|end| *end <= self.length)
            .ok_or_else(|| AppError::storage("carrier_range_invalid"))?;
        let lease = self.memory.reserve_carrier_bytes(length).await?;
        let mut output = BytesMut::with_capacity(length);
        let mut position = 0u64;
        let mut input = self.open_stream();
        while let Some(chunk) = input.next().await {
            let chunk = chunk?;
            let next = position + chunk.len() as u64;
            if next > offset && position < end {
                let start = offset.saturating_sub(position) as usize;
                let stop = (end.min(next) - position) as usize;
                output.extend_from_slice(&chunk[start..stop]);
            }
            position = next;
            if position >= end {
                break;
            }
        }
        if output.len() != length {
            return Err(AppError::storage("carrier_range_truncated"));
        }
        Ok(Bytes::from_owner(Allocation {
            bytes: output.freeze(),
            _lease: lease,
        }))
    }
}

impl Drop for BodyCarrier {
    fn drop(&mut self) {
        let Some(archive) = self.archive.clone() else {
            return;
        };
        let objects: Vec<_> = self
            .segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Object(reference) => Some(reference.object_locator.clone()),
                Segment::Ram(_) => None,
            })
            .collect();
        let staging = self.staging;
        if objects.is_empty() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for locator in objects {
                    if archive
                        .delete_archive_staging_object(staging, &locator)
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            stage = "carrier_cleanup",
                            "owned carrier object cleanup deferred"
                        );
                    }
                }
            });
        }
    }
}
