use super::*;
use std::{io::SeekFrom, path::Path};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// A replay verified against the capture's original byte length and digest.
/// The anonymous file is local working storage, never a durable source receipt.
pub(crate) struct VerifiedJsonFile {
    file: std::fs::File,
    length: usize,
    nodes: usize,
}

impl VerifiedJsonFile {
    pub(crate) fn length(&self) -> usize {
        self.length
    }
    pub(crate) fn nodes(&self) -> usize {
        self.nodes
    }
    pub(crate) fn into_file(self) -> std::fs::File {
        self.file
    }
}

impl RequestSpool {
    pub(crate) async fn verified_json_file(
        self: &Arc<Self>,
        directory: &Path,
    ) -> Result<VerifiedJsonFile, RequestSpoolReadError> {
        #[cfg(test)]
        self.read_count.fetch_add(1, Ordering::AcqRel);
        tokio::fs::create_dir_all(directory)
            .await
            .map_err(|_| RequestSpoolReadError)?;
        let file = tempfile::tempfile_in(directory).map_err(|_| RequestSpoolReadError)?;
        let mut file = tokio::fs::File::from_std(file);
        let resident = self
            .shared
            .body
            .lock()
            .map_err(|_| RequestSpoolReadError)?
            .clone();
        let mut input: std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
        > = if let Some(body) = resident {
            Box::pin(futures_util::stream::unfold(body, |body| async move {
                if body.is_empty() {
                    return None;
                }
                let size = body.len().min(REQUEST_SPOOL_CHUNK_BYTES);
                Some((Ok(body.slice(..size)), body.slice(size..)))
            }))
        } else {
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
                    let download = archive
                        .open_stream(key, None)
                        .await
                        .map_err(|_| RequestSpoolReadError)?;
                    if download.object_size != self.len() as u64 {
                        return Err(RequestSpoolReadError);
                    }
                    download.stream
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
                    Box::pin(futures_util::stream::unfold(body, |body| async move {
                        if body.is_empty() {
                            return None;
                        }
                        let size = body.len().min(REQUEST_SPOOL_CHUNK_BYTES);
                        Some((Ok(body.slice(..size)), body.slice(size..)))
                    }))
                }
            }
        };
        let mut digest = blake3::Hasher::new();
        let mut scanner = crate::gateway_body::memory::JsonMemoryScanner::default();
        let mut length = 0usize;
        let mut nodes = 1usize;
        while let Some(bytes) = input.next().await {
            let bytes = bytes.map_err(|_| RequestSpoolReadError)?;
            length = length
                .checked_add(bytes.len())
                .ok_or(RequestSpoolReadError)?;
            if length > self.len() {
                return Err(RequestSpoolReadError);
            }
            digest.update(&bytes);
            nodes = scanner.observe(&bytes);
            file.write_all(&bytes)
                .await
                .map_err(|_| RequestSpoolReadError)?;
        }
        if length != self.len() || digest.finalize().as_bytes() != &self.shared.digest {
            return Err(RequestSpoolReadError);
        }
        file.flush().await.map_err(|_| RequestSpoolReadError)?;
        file.seek(SeekFrom::Start(0))
            .await
            .map_err(|_| RequestSpoolReadError)?;
        self.release_durable_retained();
        Ok(VerifiedJsonFile {
            file: file.into_std().await,
            length,
            nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

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
        let mut replay = source.verified_json_file(directory.path()).await.unwrap();
        assert_eq!(replay.length, body.len());
        assert_eq!(
            replay.nodes,
            crate::gateway_body::memory::JsonMemoryScanner::default().observe(&body)
        );
        let mut actual = Vec::new();
        replay.file.read_to_end(&mut actual).unwrap();
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
        assert!(source.verified_json_file(directory.path()).await.is_err());
    }
}
