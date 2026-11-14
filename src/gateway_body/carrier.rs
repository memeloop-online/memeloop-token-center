//! Immutable application bodies. Only RAM allocations and typed object-store
//! objects may carry bytes; migration journals are deliberately outside this API.
use std::{
    fmt::Display,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
};

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use tokio::sync::OwnedSemaphorePermit;

use super::memory::ProxyMemoryBudget;
use crate::{
    archive::{ArchiveStore, CarrierObjectOwner, StagedArchiveObject},
    archive_staging::ArchiveStagingKey,
    error::AppError,
};

pub(crate) type CarrierStream = Pin<Box<dyn Stream<Item = Result<Bytes, AppError>> + Send>>;

pub(crate) struct CarrierCapture {
    memory: Arc<ProxyMemoryBudget>,
    archive: Option<ArchiveStore>,
    staging: ArchiveStagingKey,
    parent: Option<Arc<BodyCarrier>>,
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
            parent: None,
        }
    }

    /// Only pointer ranges owned by this sealed source may share its allocation
    /// credit. Copied or transformed bytes still acquire their own real lease.
    pub(crate) fn derived_from(source: &Arc<BodyCarrier>, staging: ArchiveStagingKey) -> Self {
        Self {
            memory: source.memory.clone(),
            archive: source.archive.clone(),
            staging,
            parent: Some(source.clone()),
        }
    }
}

// Bytes::clone/slice retain this owner, so dropping the carrier cannot release
// an allocation while a parser or upstream transport still holds its bytes.
struct Allocation {
    bytes: Bytes,
    _lease: OwnedSemaphorePermit,
    _retained: Option<OwnedSemaphorePermit>,
}
struct AllocationView(Arc<Allocation>);
impl AsRef<[u8]> for AllocationView {
    fn as_ref(&self) -> &[u8] {
        &self.0.bytes
    }
}

#[derive(Clone)]
enum Segment {
    Ram(Bytes),
    Object(StagedArchiveObject),
}

pub(crate) struct BodyCarrier {
    memory: Arc<ProxyMemoryBudget>,
    archive: Option<ArchiveStore>,
    staging: ArchiveStagingKey,
    segments: Vec<Mutex<Segment>>,
    offload: tokio::sync::Mutex<()>,
    sealed_reference: Option<StagedArchiveObject>,
    parent: Option<Arc<BodyCarrier>>,
    allocations: Mutex<Vec<Weak<Allocation>>>,
    length: u64,
    digest: String,
    owned_objects: Mutex<Vec<(CarrierObjectOwner, bool)>>,
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
        let parent = capture.parent;
        let staging = match &capture.archive {
            Some(archive) => archive.select_staging_key(capture.staging)?,
            None => capture.staging,
        };
        let mut source = Self {
            memory: capture.memory,
            archive: capture.archive,
            staging,
            segments: Vec::new(),
            offload: tokio::sync::Mutex::new(()),
            sealed_reference: None,
            parent: parent.clone(),
            allocations: Mutex::new(Vec::new()),
            length: 0,
            digest: String::new(),
            owned_objects: Mutex::new(Vec::new()),
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
                if parent.as_ref().is_some_and(|parent| parent.owns_ram(&part)) {
                    source.segments.push(Mutex::new(Segment::Ram(part)));
                    continue;
                }
                let mut retry = std::time::Duration::from_millis(100);
                let (lease, retained) = loop {
                    if let Some((lease, retained)) =
                        source.memory.try_reserve_carrier_ingress(part.len())
                    {
                        break (lease, Some(retained));
                    }
                    match source.offload_one().await {
                        Ok(true) => continue,
                        Ok(false) => {
                            break (source.reserve_workspace(part.len()).await?, None);
                        }
                        Err(_) => {
                            // Remote failure cannot discard the sole source. If
                            // real RAM is available retain it and continue;
                            // otherwise stop polling input and retry this owned
                            // prefix, without a request deadline or fake seal.
                            if let Some(lease) = source.memory.try_reserve_ingress_bytes(part.len())
                            {
                                break (lease, None);
                            }
                            tokio::time::sleep(retry).await;
                            retry = (retry * 2).min(std::time::Duration::from_secs(300));
                        }
                    }
                };
                let bytes = source.leased_bytes(Bytes::copy_from_slice(&part), lease, retained);
                source.segments.push(Mutex::new(Segment::Ram(bytes)));
            }
        }
        source.digest = hash.finalize().to_hex().to_string();
        source.sealed_reference = match source.segments.as_slice() {
            [segment] => match &*segment.lock().unwrap() {
                Segment::Object(reference) => Some(reference.clone()),
                Segment::Ram(_) => None,
            },
            _ => None,
        };
        Ok(Arc::new(source))
    }

    fn leased_bytes(
        &self,
        bytes: Bytes,
        lease: OwnedSemaphorePermit,
        retained: Option<OwnedSemaphorePermit>,
    ) -> Bytes {
        let allocation = Arc::new(Allocation {
            bytes,
            _lease: lease,
            _retained: retained,
        });
        let mut owners = self.allocations.lock().unwrap();
        // Discard expired identities as new transient replay buffers arrive.
        // Weak entries never pin bytes or their allocation credit.
        owners.retain(|owner| owner.strong_count() != 0);
        owners.push(Arc::downgrade(&allocation));
        Bytes::from_owner(AllocationView(allocation))
    }

    fn owns_ram(&self, bytes: &Bytes) -> bool {
        let start = bytes.as_ptr() as usize;
        let Some(end) = start.checked_add(bytes.len()) else {
            return false;
        };
        let owned_here = self.allocations.lock().unwrap().iter().any(|owner| {
            let Some(allocation) = owner.upgrade() else {
                return false;
            };
            let base = allocation.bytes.as_ptr() as usize;
            base.checked_add(allocation.bytes.len())
                .is_some_and(|limit| start >= base && end <= limit)
        });
        owned_here
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.owns_ram(bytes))
    }

    async fn offload_one(&self) -> Result<bool, AppError> {
        let _owner = self.offload.lock().await;
        let Some(archive) = &self.archive else {
            return Ok(false);
        };
        let Some(index) = self
            .segments
            .iter()
            .position(|segment| matches!(&*segment.lock().unwrap(), Segment::Ram(_)))
        else {
            return Ok(false);
        };
        let Segment::Ram(bytes) = self.segments[index].lock().unwrap().clone() else {
            unreachable!()
        };
        let locator = format!(
            "{}/carrier-{}",
            self.staging.canonical_prefix(),
            uuid::Uuid::now_v7()
        );
        // Register exact ownership before any remote operation: a finish whose
        // acknowledgement is lost must not orphan an object outside cleanup.
        let owner = CarrierObjectOwner::new(self.staging, locator.clone(), &bytes)?;
        self.owned_objects
            .lock()
            .unwrap()
            .push((owner.clone(), false));
        archive.begin_carrier_object(&owner).await?;
        let mut writer = archive.start_writer(&locator).await?;
        if let Err(error) = writer.write(bytes.clone()).await {
            let _ = writer.abort().await;
            return Err(error);
        }
        let staged = writer.finish_staged().await?;
        if staged.size_bytes != bytes.len() as u64
            || staged.blake3_digest != blake3::hash(&bytes).to_hex().as_str()
        {
            return Err(AppError::storage("carrier_stage_identity_mismatch"));
        }
        // Only a confirmed Complete response plus exact identity authorizes
        // release cleanup. Unknown/cancelled completions retain their durable
        // active receipt: HEAD absence cannot prove no late completion.
        if let Some((_, settled)) = self
            .owned_objects
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(candidate, _)| candidate == &owner)
        {
            *settled = true;
        }
        // A complete object, not an UploadPart ACK, is now replayable. Only
        // replacing this segment releases the unique RAM allocation lease.
        *self.segments[index].lock().unwrap() = Segment::Object(staged);
        Ok(true)
    }

    // Never join the semaphore queue while this source can release its own
    // allocations. One uploader owns each replacement; failed publication keeps
    // RAM and its lease intact and retries without discarding the source.
    async fn reserve_workspace(&self, bytes: usize) -> Result<OwnedSemaphorePermit, AppError> {
        let mut retry = std::time::Duration::from_millis(100);
        loop {
            if let Some(lease) = self.memory.try_reserve_ingress_bytes(bytes) {
                return Ok(lease);
            }
            match self.offload_one().await {
                Ok(true) => continue,
                Ok(false) => {
                    // A derived alias can retain the same allocation in its
                    // parent. Release that owner too before waiting for credit.
                    let mut parent = self.parent.as_ref();
                    let mut released = false;
                    while let Some(source) = parent {
                        match source.offload_one().await {
                            Ok(true) => {
                                released = true;
                                break;
                            }
                            Ok(false) => parent = source.parent.as_ref(),
                            Err(_) => {
                                tokio::time::sleep(retry).await;
                                retry = (retry * 2).min(std::time::Duration::from_secs(300));
                                released = true;
                                break;
                            }
                        }
                    }
                    if released {
                        continue;
                    }
                    return self.memory.reserve_carrier_bytes(bytes).await;
                }
                Err(_) => {
                    tokio::time::sleep(retry).await;
                    retry = (retry * 2).min(std::time::Duration::from_secs(300));
                }
            }
        }
    }

    /// Reserve a fixed metadata workspace after releasing this source's own
    /// offloadable allocations. The caller retains the permit through all
    /// resident scratch pages and publication buffers.
    pub(crate) async fn reserve_metadata_workspace(
        self: &Arc<Self>,
        bytes: usize,
    ) -> Result<OwnedSemaphorePermit, AppError> {
        self.reserve_workspace(bytes).await
    }

    pub(crate) fn len(&self) -> u64 {
        self.length
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn sealed_staged_reference(&self) -> Option<&StagedArchiveObject> {
        self.sealed_reference.as_ref()
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
                    let segment = source.segments.get(index)?.lock().unwrap().clone();
                    index += 1;
                    match &segment {
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
                            let _ = archive;
                            let reference = reference.clone();
                            remote = Some(Box::pin(stream::try_unfold(
                                (source.clone(), reference, 0u64),
                                |(source, reference, offset)| async move {
                                    if offset == reference.size_bytes {
                                        return Ok(None);
                                    }
                                    let count =
                                        (reference.size_bytes - offset).min(64 * 1024) as usize;
                                    // Reserve application destination before receiving the SDK
                                    // frame. Each HTTP range is bounded independently; no slow
                                    // consumer retains the backing of an entire object stream.
                                    let lease = source.reserve_workspace(count).await?;
                                    let archive = source.archive.as_ref().ok_or_else(|| {
                                        AppError::storage("carrier_backend_missing")
                                    })?;
                                    let mut download = archive
                                        .open_stream(
                                            &reference.object_locator,
                                            Some(offset..offset + count as u64),
                                        )
                                        .await?
                                        .stream;
                                    let mut output = Vec::with_capacity(count);
                                    while let Some(chunk) = download.next().await {
                                        let chunk = chunk.map_err(|_| {
                                            AppError::storage("carrier_object_read_failed")
                                        })?;
                                        if chunk.len() > count.saturating_sub(output.len()) {
                                            return Err(AppError::storage(
                                                "carrier_object_read_failed",
                                            ));
                                        }
                                        output.extend_from_slice(&chunk);
                                    }
                                    if output.len() != count {
                                        return Err(AppError::storage(
                                            "carrier_object_read_failed",
                                        ));
                                    }
                                    drop(download);
                                    let owned =
                                        source.leased_bytes(Bytes::from(output), lease, None);
                                    Ok(Some((owned, (source, reference, offset + count as u64))))
                                },
                            )));
                        }
                    }
                }
            },
        ))
    }

    /// Release only confirmed offloaded allocation owners. Callers can cancel
    /// this wait by dropping the future; failed writes retain the RAM source.
    pub(crate) async fn seal_to_objects(self: &Arc<Self>) -> Result<(), AppError> {
        if self.archive.is_none() {
            return Err(AppError::internal("carrier_object_capability_missing"));
        }
        loop {
            match self.offload_one().await {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            }
        }
    }

    /// Bounded independently-owned replay output; its lease follows Bytes
    /// clones/slices rather than the caller's next poll.
    pub(crate) async fn read_leased_chunk(
        self: &Arc<Self>,
        offset: u64,
        maximum: usize,
    ) -> Result<Bytes, AppError> {
        let remaining = self
            .length
            .checked_sub(offset)
            .ok_or_else(|| AppError::internal("carrier_range_invalid"))?;
        let count = remaining.min(maximum.min(64 * 1024) as u64) as usize;
        if count == 0 {
            return Ok(Bytes::new());
        }
        let lease = self.reserve_workspace(count).await?;
        let mut output = vec![0; count];
        self.read_into(offset, &mut output).await?;
        Ok(self.leased_bytes(Bytes::from(output), lease, None))
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
        if length == 0 {
            return Ok(Bytes::new());
        }
        let mut position = 0u64;
        for segment in &self.segments {
            let segment = segment.lock().unwrap().clone();
            let size = match &segment {
                Segment::Ram(bytes) => bytes.len() as u64,
                Segment::Object(reference) => reference.size_bytes,
            };
            if offset >= position && end <= position + size {
                if let Segment::Ram(bytes) = &segment {
                    return Ok(bytes.slice((offset - position) as usize..(end - position) as usize));
                }
            }
            position += size;
        }
        if length > self.memory.snapshot().1 || self.archive.is_none() {
            return Err(AppError::internal(
                "carrier_contiguous_read_requires_stream_or_owned_workspace",
            ));
        }
        let lease = self.reserve_workspace(length).await?;
        let mut output = vec![0; length];
        self.read_into(offset, &mut output).await?;
        Ok(self.leased_bytes(Bytes::from(output), lease, None))
    }

    /// The caller supplies and owns this workspace. RAM slices copy directly;
    /// no second allocation or semaphore acquisition can depend on this source.
    pub(crate) async fn read_into(
        self: &Arc<Self>,
        offset: u64,
        output: &mut [u8],
    ) -> Result<(), AppError> {
        let length = output.len();
        let end = offset
            .checked_add(length as u64)
            .filter(|end| *end <= self.length)
            .ok_or_else(|| AppError::storage("carrier_range_invalid"))?;
        let mut position = 0u64;
        let mut written = 0usize;
        for segment in &self.segments {
            let segment = segment.lock().unwrap().clone();
            let size = match &segment {
                Segment::Ram(bytes) => bytes.len() as u64,
                Segment::Object(reference) => reference.size_bytes,
            };
            let next = position + size;
            if next > offset && position < end {
                let start = offset.saturating_sub(position);
                let stop = end.min(next) - position;
                match &segment {
                    Segment::Ram(bytes) => {
                        let bytes = &bytes[start as usize..stop as usize];
                        output[written..written + bytes.len()].copy_from_slice(bytes);
                        written += bytes.len();
                    }
                    Segment::Object(reference) => {
                        let archive = self
                            .archive
                            .as_ref()
                            .ok_or_else(|| AppError::storage("carrier_backend_missing"))?;
                        let mut download = archive
                            .open_stream(&reference.object_locator, Some(start..stop))
                            .await?
                            .stream;
                        let expected = written + (stop - start) as usize;
                        while let Some(chunk) = download.next().await {
                            let chunk = chunk
                                .map_err(|_| AppError::storage("carrier_object_read_failed"))?;
                            if chunk.len() > expected.saturating_sub(written) {
                                return Err(AppError::storage("carrier_range_overrun"));
                            }
                            output[written..written + chunk.len()].copy_from_slice(&chunk);
                            written += chunk.len();
                        }
                        if written != expected {
                            return Err(AppError::storage("carrier_range_truncated"));
                        }
                    }
                }
            }
            position = next;
            if position >= end {
                break;
            }
        }
        if written != length {
            return Err(AppError::storage("carrier_range_truncated"));
        }
        Ok(())
    }
}

impl Drop for BodyCarrier {
    fn drop(&mut self) {
        let Some(archive) = self.archive.clone() else {
            return;
        };
        let objects = std::mem::take(self.owned_objects.get_mut().unwrap());
        if objects.is_empty() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for (owner, settled) in objects {
                    if !settled {
                        continue;
                    }
                    if archive.release_carrier_object(&owner).await.is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_staging::{ArchiveStagingOwner, ArchiveStagingPurpose};

    fn key() -> ArchiveStagingKey {
        ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
            ArchiveStagingPurpose::Request,
            uuid::Uuid::now_v7(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn range_slice_and_stream_keep_real_allocation_after_source_drop() {
        let memory = Arc::new(ProxyMemoryBudget::new(1024 * 1024));
        let source = BodyCarrier::capture(
            stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(
                b"immutable raw body",
            ))]),
            CarrierCapture::new(memory.clone(), None, key()),
        )
        .await
        .unwrap();
        let bytes = source.read_range(10, 3).await.unwrap();
        assert_eq!(bytes, b"raw"[..]);
        let mut replay = source.open_stream();
        drop(source);
        assert!(memory.snapshot().0 > 0);
        assert_eq!(
            replay.next().await.unwrap().unwrap(),
            b"immutable raw body"[..]
        );
        drop(replay);
        assert!(memory.snapshot().0 > 0); // the returned range is still an owner
        drop(bytes);
        assert_eq!(memory.snapshot().0, 0);
    }

    #[tokio::test]
    async fn derived_alias_shares_allocation_but_copied_bytes_are_charged() {
        let memory = Arc::new(ProxyMemoryBudget::new(1024 * 1024));
        let source = BodyCarrier::capture(
            stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(
                b"raw WAV bytes",
            ))]),
            CarrierCapture::new(memory.clone(), None, key()),
        )
        .await
        .unwrap();
        let original_charge = memory.snapshot().0;
        let slice = source.read_range(4, 3).await.unwrap();
        let derived = BodyCarrier::capture(
            stream::iter([Ok::<_, std::io::Error>(slice)]),
            CarrierCapture::derived_from(&source, key()),
        )
        .await
        .unwrap();
        assert_eq!(memory.snapshot().0, original_charge);
        let copied = BodyCarrier::capture(
            stream::iter([Ok::<_, std::io::Error>(Bytes::copy_from_slice(b"WAV"))]),
            CarrierCapture::derived_from(&source, key()),
        )
        .await
        .unwrap();
        assert!(memory.snapshot().0 > original_charge);
        drop(copied);
        drop(source);
        assert_eq!(memory.snapshot().0, original_charge);
        assert_eq!(derived.read_range(0, 3).await.unwrap(), b"WAV"[..]);
        drop(derived);
        assert_eq!(memory.snapshot().0, 0);
    }

    #[tokio::test]
    async fn ram_only_full_source_reads_into_caller_workspace_and_streams() {
        let memory = Arc::new(ProxyMemoryBudget::new(128 * 1024));
        let source = BodyCarrier::capture(
            stream::iter([
                Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; 65536])),
                Ok(Bytes::from(vec![b'b'; 65536])),
            ]),
            CarrierCapture::new(memory.clone(), None, key()),
        )
        .await
        .unwrap();
        assert_eq!(memory.snapshot().0, 128 * 1024);
        let mut header = [0; 12];
        source.read_into(65530, &mut header).await.unwrap();
        assert_eq!(&header, b"aaaaaabbbbbb");
        assert_eq!(memory.snapshot().0, 128 * 1024);
        let mut replay = source.open_stream();
        let mut hash = blake3::Hasher::new();
        while let Some(bytes) = replay.next().await {
            hash.update(&bytes.unwrap());
        }
        assert_eq!(hash.finalize().to_hex().as_str(), source.digest());
        drop(replay);
        drop(source);
        assert_eq!(memory.snapshot().0, 0);
    }

    async fn full_ram_source(memory: Arc<ProxyMemoryBudget>) -> Arc<BodyCarrier> {
        let mut source = BodyCarrier::capture(
            stream::iter([
                Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; 65536])),
                Ok(Bytes::from(vec![b'b'; 65536])),
            ]),
            CarrierCapture::new(memory, None, key()),
        )
        .await
        .unwrap();
        Arc::get_mut(&mut source).unwrap().archive = Some(ArchiveStore::memory_for_test());
        source
    }

    #[tokio::test]
    async fn sealed_cross_segment_range_releases_its_own_full_budget() {
        let memory = Arc::new(ProxyMemoryBudget::new(128 * 1024));
        let source = full_ram_source(memory.clone()).await;
        assert_eq!(memory.snapshot().0, 128 * 1024);
        let bytes = source.read_range(65530, 12).await.unwrap();
        assert_eq!(bytes, b"aaaaaabbbbbb"[..]);
        assert_eq!(source.len(), 128 * 1024);
        drop(bytes);
        drop(source);
        assert_eq!(memory.snapshot().0, 0);
    }

    #[tokio::test]
    async fn offloaded_parent_keeps_live_reader_allocation_provenance() {
        let memory = Arc::new(ProxyMemoryBudget::new(128 * 1024));
        let source = full_ram_source(memory.clone()).await;
        let reader = source.read_range(0, 65536).await.unwrap();
        assert!(source.offload_one().await.unwrap());
        // The segment is now remote, but this old reader still owns its RAM.
        assert_eq!(memory.snapshot().0, 128 * 1024);
        let derived = BodyCarrier::capture(
            stream::iter([Ok::<_, std::io::Error>(reader.slice(4..12))]),
            CarrierCapture::derived_from(&source, key()),
        )
        .await
        .unwrap();
        assert_eq!(memory.snapshot().0, 128 * 1024);
        assert_eq!(derived.read_range(0, 8).await.unwrap(), b"aaaaaaaa"[..]);
        drop(reader);
        drop(source);
        assert_eq!(memory.snapshot().0, 128 * 1024);
        drop(derived);
        assert_eq!(memory.snapshot().0, 0);
    }

    #[tokio::test]
    async fn derived_cross_segment_range_releases_both_alias_owners() {
        let memory = Arc::new(ProxyMemoryBudget::new(128 * 1024));
        let source = full_ram_source(memory.clone()).await;
        let derived = BodyCarrier::capture(
            source.open_stream(),
            CarrierCapture::derived_from(&source, key()),
        )
        .await
        .unwrap();
        assert_eq!(memory.snapshot().0, 128 * 1024);
        assert_eq!(
            derived.read_range(65530, 12).await.unwrap(),
            b"aaaaaabbbbbb"[..]
        );
        assert_eq!(
            source.read_range(65530, 12).await.unwrap(),
            b"aaaaaabbbbbb"[..]
        );
        drop(derived);
        drop(source);
        assert_eq!(memory.snapshot().0, 0);
    }

    #[tokio::test]
    async fn object_replay_offloads_other_sealed_ram_before_waiting_for_credit() {
        let memory = Arc::new(ProxyMemoryBudget::new(128 * 1024));
        let source = full_ram_source(memory.clone()).await;
        assert!(source.offload_one().await.unwrap());
        let external = memory.try_reserve_ingress_bytes(65536).unwrap();
        assert_eq!(memory.snapshot().0, 128 * 1024);
        let mut replay = source.open_stream();
        let first = replay.next().await.unwrap().unwrap();
        assert_eq!(first, vec![b'a'; 65536]);
        drop(first);
        let second = replay.next().await.unwrap().unwrap();
        assert_eq!(second, vec![b'b'; 65536]);
        drop(second);
        assert!(replay.next().await.is_none());
        drop(replay);
        drop(source);
        drop(external);
        assert_eq!(memory.snapshot().0, 0);
    }

    #[tokio::test]
    async fn pressure_seals_readable_objects_without_local_files_and_replays_ranges() {
        let memory = Arc::new(ProxyMemoryBudget::new(256 * 1024));
        let archive = ArchiveStore::memory_for_test();
        let body = vec![
            Bytes::from(vec![b'a'; 65536]),
            Bytes::from(vec![b'b'; 65536]),
            Bytes::from(vec![b'c'; 65536]),
        ];
        let expected_hash = {
            let mut hash = blake3::Hasher::new();
            for chunk in &body {
                hash.update(chunk);
            }
            hash.finalize().to_hex().to_string()
        };
        let source = BodyCarrier::capture(
            stream::iter(body.into_iter().map(Ok::<_, std::io::Error>)),
            CarrierCapture::new(memory.clone(), Some(archive), key()),
        )
        .await
        .unwrap();
        assert_eq!(source.len(), 3 * 65536);
        assert_eq!(source.digest(), expected_hash);
        assert!(
            source
                .segments
                .iter()
                .any(|segment| matches!(&*segment.lock().unwrap(), Segment::Object(_)))
        );
        assert_eq!(
            source.read_range(65530, 12).await.unwrap(),
            b"aaaaaabbbbbb"[..]
        );
        let mut replay = source.open_stream();
        let mut hash = blake3::Hasher::new();
        let mut size = 0;
        while let Some(chunk) = replay.next().await {
            let chunk = chunk.unwrap();
            size += chunk.len();
            hash.update(&chunk);
        }
        assert_eq!(size, source.len() as usize);
        assert_eq!(hash.finalize().to_hex().as_str(), source.digest());
    }
}
