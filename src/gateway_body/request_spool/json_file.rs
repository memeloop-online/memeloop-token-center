use super::*;
use std::{
    io::{Read, SeekFrom},
    path::Path,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// A replay verified against the capture's original byte length and digest.
/// Local working storage or the captured resident owner; never a durable receipt.
pub(crate) struct VerifiedJsonInput {
    reader: Box<dyn Read + Send + Sync>,
    length: usize,
    nodes: usize,
}

impl VerifiedJsonInput {
    pub(crate) fn length(&self) -> usize {
        self.length
    }
    pub(crate) fn nodes(&self) -> usize {
        self.nodes
    }
    pub(crate) fn into_reader(self) -> Box<dyn Read + Send + Sync> {
        self.reader
    }
}

/// Keeps both the captured bytes and their existing permits together while a
/// cache eviction or a completed background Put releases the shared reference.
struct ResidentReplay {
    body: Bytes,
    offset: usize,
    residency: Arc<BodyResidency>,
    has_working_credit: bool,
}

impl Read for ResidentReplay {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = output.len().min(self.body.len() - self.offset);
        output[..count].copy_from_slice(&self.body[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

impl Drop for ResidentReplay {
    fn drop(&mut self) {
        // Release our byte reference before making its permits reclaimable.
        self.body = Bytes::new();
        self.residency.replay_readers.fetch_sub(1, Ordering::AcqRel);
        self.residency.release_if_idle();
    }
}

#[derive(Default)]
struct Verification {
    digest: blake3::Hasher,
    scanner: crate::gateway_body::memory::JsonMemoryScanner,
    length: usize,
    nodes: usize,
}

impl Verification {
    fn observe(&mut self, bytes: &[u8], expected: usize) -> Result<(), RequestSpoolReadError> {
        self.length = self
            .length
            .checked_add(bytes.len())
            .ok_or(RequestSpoolReadError)?;
        if self.length > expected {
            return Err(RequestSpoolReadError);
        }
        self.digest.update(bytes);
        self.nodes = self.scanner.observe(bytes);
        Ok(())
    }

    fn finish(
        self,
        source: &RequestSpool,
        reader: impl Read + Send + Sync + 'static,
    ) -> Result<VerifiedJsonInput, RequestSpoolReadError> {
        if self.length != source.len() || self.digest.finalize().as_bytes() != &source.shared.digest
        {
            return Err(RequestSpoolReadError);
        }
        Ok(VerifiedJsonInput {
            reader: Box::new(reader),
            length: self.length,
            nodes: self.nodes.max(1),
        })
    }
}

impl RequestSpool {
    fn resident_replay(&self) -> Result<Option<ResidentReplay>, RequestSpoolReadError> {
        let body = self.shared.body.lock().map_err(|_| RequestSpoolReadError)?;
        let Some(body) = body.as_ref() else {
            return Ok(None);
        };
        let has_working_credit = self
            .shared
            .residency
            .metadata
            .lock()
            .map_err(|_| RequestSpoolReadError)?
            .is_some();
        // Eviction takes the body mutex before releasing residency. Increment
        // inside that same critical section, before cloning its byte owner.
        self.shared
            .residency
            .replay_readers
            .fetch_add(1, Ordering::AcqRel);
        Ok(Some(ResidentReplay {
            body: body.clone(),
            offset: 0,
            residency: self.shared.residency.clone(),
            has_working_credit,
        }))
    }

    pub(crate) async fn verified_json_file(
        self: &Arc<Self>,
        directory: &Path,
        budget: &crate::gateway_body::memory::ProxyMemoryBudget,
    ) -> Result<VerifiedJsonInput, RequestSpoolReadError> {
        #[cfg(test)]
        self.read_count.fetch_add(1, Ordering::AcqRel);
        // One replay borrows this source's existing 64 KiB metadata workspace.
        // Repeated readers share that credit rather than charging it twice.
        let _replay = self.shared.residency.replay_lock.lock().await;
        let resident = self.resident_replay()?;
        let _working = if resident
            .as_ref()
            .is_some_and(|body| body.has_working_credit)
        {
            None
        } else {
            // Local/remote file sources hold no resident capture allocation.
            // This covers a range response, the read buffer and file I/O buffers.
            Some(
                budget
                    .streaming_working_set(4 * REQUEST_SPOOL_CHUNK_BYTES)
                    .await
                    .map_err(|_| RequestSpoolReadError)?,
            )
        };
        let mut verification = Verification::default();
        if resident.is_none()
            && let Some(local) = self.shared.local.as_ref()
        {
            let mut file =
                tokio::fs::File::from_std(local.reopen().map_err(|_| RequestSpoolReadError)?);
            file.set_max_buf_size(REQUEST_SPOOL_CHUNK_BYTES);
            let mut buffer = vec![0u8; REQUEST_SPOOL_CHUNK_BYTES];
            loop {
                let count = file
                    .read(&mut buffer)
                    .await
                    .map_err(|_| RequestSpoolReadError)?;
                if count == 0 {
                    break;
                }
                verification.observe(&buffer[..count], self.len())?;
            }
            file.seek(SeekFrom::Start(0))
                .await
                .map_err(|_| RequestSpoolReadError)?;
            return verification.finish(self, file.into_std().await);
        }

        if let Some(resident) = resident {
            for bytes in resident.body.chunks(REQUEST_SPOOL_CHUNK_BYTES) {
                verification.observe(bytes, self.len())?;
                tokio::task::yield_now().await;
            }
            // A working file lets parsing release the durable RAM cache. If
            // local storage is unavailable, parse the original resident owner.
            let file = copy_resident_file(directory, &resident.body).await;
            let verified = match file {
                Ok(file) => verification.finish(self, file)?,
                Err(_) => verification.finish(self, resident)?,
            };
            self.release_durable_retained();
            return Ok(verified);
        }
        tokio::fs::create_dir_all(directory)
            .await
            .map_err(|_| RequestSpoolReadError)?;
        let file = tempfile::tempfile_in(directory).map_err(|_| RequestSpoolReadError)?;
        let mut file = tokio::fs::File::from_std(file);
        file.set_max_buf_size(REQUEST_SPOOL_CHUNK_BYTES);
        if !self.shared.durable_ok.load(Ordering::Acquire) && !self.upload_outcome().await {
            return Err(RequestSpoolReadError);
        }
        let key = self
            .shared
            .object_key
            .as_deref()
            .ok_or(RequestSpoolReadError)?;
        match self.shared.durable.as_ref().ok_or(RequestSpoolReadError)? {
            DurableRequestBodies::Archive(archive) => {
                // Bound the actual object-store request, not slices of an
                // unbounded response chunk. open_stream validates the
                // returned range and EOF before accepting its final chunk.
                let mut offset = 0u64;
                while offset < self.len() as u64 {
                    let end = offset
                        .saturating_add(REQUEST_SPOOL_CHUNK_BYTES as u64)
                        .min(self.len() as u64);
                    let mut download = archive
                        .open_stream(key, Some(offset..end))
                        .await
                        .map_err(|_| RequestSpoolReadError)?;
                    if download.object_size != self.len() as u64 {
                        return Err(RequestSpoolReadError);
                    }
                    while let Some(bytes) = download.stream.next().await {
                        let bytes = bytes.map_err(|_| RequestSpoolReadError)?;
                        verification.observe(&bytes, self.len())?;
                        file.write_all(&bytes)
                            .await
                            .map_err(|_| RequestSpoolReadError)?;
                    }
                    offset = end;
                }
            }
            #[cfg(test)]
            DurableRequestBodies::Memory(store) => {
                store.reads.fetch_add(1, Ordering::AcqRel);
                let body = store
                    .objects
                    .lock()
                    .map_err(|_| RequestSpoolReadError)?
                    .get(key)
                    .cloned()
                    .ok_or(RequestSpoolReadError)?;
                for bytes in body.chunks(REQUEST_SPOOL_CHUNK_BYTES) {
                    verification.observe(bytes, self.len())?;
                    file.write_all(bytes)
                        .await
                        .map_err(|_| RequestSpoolReadError)?;
                }
            }
        }
        file.flush().await.map_err(|_| RequestSpoolReadError)?;
        file.seek(SeekFrom::Start(0))
            .await
            .map_err(|_| RequestSpoolReadError)?;
        let verified = verification.finish(self, file.into_std().await)?;
        self.release_durable_retained();
        Ok(verified)
    }
}

async fn copy_resident_file(directory: &Path, body: &[u8]) -> std::io::Result<std::fs::File> {
    tokio::fs::create_dir_all(directory).await?;
    let mut file = tokio::fs::File::from_std(tempfile::tempfile_in(directory)?);
    file.set_max_buf_size(REQUEST_SPOOL_CHUNK_BYTES);
    for bytes in body.chunks(REQUEST_SPOOL_CHUNK_BYTES) {
        file.write_all(bytes).await?;
    }
    file.flush().await?;
    file.seek(SeekFrom::Start(0)).await?;
    Ok(file.into_std().await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_body::memory::ProxyMemoryBudget;
    use std::io::Read;
    use std::time::Duration;

    #[tokio::test]
    async fn resident_replay_finishes_with_full_budget_and_pending_put() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let release_put = store.pause_next_write();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let memory = budget.reservation();
        let body = Bytes::from(format!("{{\"input\":\"{}\"}}", "a".repeat(128 * 1024)));
        let source = Arc::new(
            admission
                .capture_with_memory(
                    Body::from(body.clone()),
                    body.len(),
                    Some(body.len()),
                    Some(&memory),
                    0,
                    None,
                )
                .await
                .unwrap(),
        );
        let used = budget.snapshot().0;
        let occupied = budget.temporary(1024 * 1024 - used).unwrap();
        assert_eq!(budget.snapshot().0, 1024 * 1024);
        let mut replay = tokio::time::timeout(
            Duration::from_secs(2),
            source.verified_json_file(directory.path(), &budget),
        )
        .await
        .expect("replay uses the capture's existing workspace")
        .unwrap();
        assert!(!source.shared.durable_ok.load(Ordering::Acquire));
        let mut actual = Vec::new();
        replay.reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, body);
        release_put.send(()).unwrap();
        assert!(source.upload_outcome().await);
        source.release_durable_retained();
        drop(occupied);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[tokio::test]
    async fn unwritable_working_directory_keeps_resident_source_and_credit_until_parse_ends() {
        let directory = tempfile::tempdir().unwrap();
        let invalid_directory = directory.path().join("regular-file");
        std::fs::write(&invalid_directory, b"not a directory").unwrap();
        let admission = RequestSpoolAdmission::new(invalid_directory.clone());
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let memory = budget.reservation();
        let body = Bytes::from_static(b"{\"input\":\"resident\"}");
        let source = Arc::new(
            admission
                .capture_with_memory(
                    Body::from(body.clone()),
                    body.len(),
                    Some(body.len()),
                    Some(&memory),
                    0,
                    None,
                )
                .await
                .unwrap(),
        );
        let charged = budget.snapshot().0;
        let occupied = budget.temporary(1024 * 1024 - charged).unwrap();
        let mut replay = tokio::time::timeout(
            Duration::from_secs(2),
            source.verified_json_file(&invalid_directory, &budget),
        )
        .await
        .expect("RAM replay is independent of directory availability")
        .unwrap();
        source.release_retained();
        drop(source);
        drop(occupied);
        assert_eq!(budget.snapshot().0, charged);
        let mut actual = Vec::new();
        replay.reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, body);
        drop(replay);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[tokio::test]
    async fn local_spill_replay_uses_existing_file_without_archive_or_new_directory() {
        let directory = tempfile::tempdir().unwrap();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned());
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let occupied = budget.temporary(1024 * 1024).unwrap();
        let memory = budget.reservation();
        let body = Bytes::from(format!("{{\"input\":\"{}\"}}", "a".repeat(256 * 1024)));
        let source = Arc::new(
            admission
                .capture_with_memory(
                    Body::from(body.clone()),
                    body.len(),
                    Some(body.len()),
                    Some(&memory),
                    0,
                    None,
                )
                .await
                .unwrap(),
        );
        assert!(source.shared.local.is_some());
        assert!(!source.upload_outcome().await);
        drop(occupied);
        let invalid_directory = directory.path().join("regular-file");
        std::fs::write(&invalid_directory, b"not a directory").unwrap();
        let mut replay = source
            .verified_json_file(&invalid_directory, &budget)
            .await
            .unwrap();
        assert_eq!(budget.snapshot().0, 0);
        let mut actual = Vec::new();
        replay.reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, body);
        assert!(!source.shared.durable_ok.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn evicted_archive_source_replays_across_multiple_range_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::config::Config::for_test("sqlite::memory:".into());
        let archive = crate::archive::ArchiveStore::from_config(&config)
            .await
            .unwrap();
        let admission =
            RequestSpoolAdmission::new(directory.path().to_owned()).with_archive(archive);
        let body = Bytes::from(format!(
            "{{\"input\":\"{}\",\"last\":true}}",
            "a".repeat(REQUEST_SPOOL_CHUNK_BYTES * 3 + 17)
        ));
        let source = Arc::new(
            admission
                .capture(Body::from(body.clone()), body.len(), Some(body.len()))
                .await
                .unwrap(),
        );
        assert!(source.upload_outcome().await);
        source.release_durable_retained();
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let mut replay = source
            .verified_json_file(directory.path(), &budget)
            .await
            .unwrap();
        let mut actual = Vec::new();
        replay.reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, body);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[tokio::test]
    async fn verified_file_preserves_original_bytes_and_incremental_shape() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let admission =
            RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store);
        let body = Bytes::from(format!(
            "{{\"input\":\"{}\",\"values\":[1,2,3]}}",
            "a".repeat(REQUEST_SPOOL_CHUNK_BYTES * 3)
        ));
        let source = Arc::new(
            admission
                .capture(Body::from(body.clone()), body.len(), Some(body.len()))
                .await
                .unwrap(),
        );
        assert!(source.upload_outcome().await);
        source.release_durable_retained();
        let mut replay = source
            .verified_json_file(
                directory.path(),
                &crate::gateway_body::memory::ProxyMemoryBudget::new(1024 * 1024),
            )
            .await
            .unwrap();
        assert_eq!(replay.length, body.len());
        assert_eq!(
            replay.nodes,
            crate::gateway_body::memory::JsonMemoryScanner::default().observe(&body)
        );
        let mut actual = Vec::new();
        replay.reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, body);
    }

    #[tokio::test]
    async fn corrupt_source_never_becomes_a_verified_parse_input() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryObjectStore::new();
        let admission = RequestSpoolAdmission::new(directory.path().to_owned())
            .with_memory_store(store.clone());
        let source = Arc::new(
            admission
                .capture(Body::from("original"), 8, Some(8))
                .await
                .unwrap(),
        );
        assert!(source.upload_outcome().await);
        source.release_durable_retained();
        store.objects.lock().unwrap().insert(
            source.object_locator().unwrap().to_owned(),
            Bytes::from_static(b"modified"),
        );
        assert!(
            source
                .verified_json_file(
                    directory.path(),
                    &crate::gateway_body::memory::ProxyMemoryBudget::new(1024 * 1024)
                )
                .await
                .is_err()
        );
    }
}
