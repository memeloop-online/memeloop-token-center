//! Process-owned RAM index for decoded JSON keys and output spans.
use super::{Result, Span};
#[cfg(test)]
use std::io::Cursor;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

trait IndexStorage: Read + Write + Seek + Send {}
impl<T: Read + Write + Seek + Send> IndexStorage for T {}

pub(crate) struct Index {
    file: Box<dyn IndexStorage>,
    end: u64,
    pub(crate) cancelled: Arc<AtomicBool>,
}

// Zero is the null pointer; records start after a one-byte sentinel.
impl Index {
    #[cfg(test)]
    pub(crate) fn memory() -> Result<Self> {
        Self::new(Box::new(Cursor::new(Vec::new())))
    }

    fn new(mut file: Box<dyn IndexStorage>) -> Result<Self> {
        file.write_all(&[0]).map_err(|_| "compact_file_io")?;
        Ok(Self {
            file,
            end: 1,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn scratch(
        scope: crate::archive::scratch_scope::ScratchScope,
        lease: tokio::sync::OwnedSemaphorePermit,
        runtime: tokio::runtime::Handle,
        cancelled: Arc<AtomicBool>,
        wake: Arc<tokio::sync::Notify>,
    ) -> Result<Self> {
        let backend = ScratchBackend {
            scope,
            runtime,
            cancelled,
            wake,
            _lease: lease,
        };
        Self::new(Box::new(super::pager::Pager::new(Box::new(backend))))
    }

    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> Result<u64> {
        let at = self.end;
        self.file
            .seek(SeekFrom::Start(at))
            .map_err(|_| "compact_file_io")?;
        self.file.write_all(bytes).map_err(|_| "compact_file_io")?;
        self.end = self
            .end
            .checked_add(bytes.len() as u64)
            .ok_or("compact_file_io")?;
        Ok(at)
    }

    fn read<const N: usize>(&mut self, at: u64) -> Result<[u8; N]> {
        let mut bytes = [0; N];
        self.file
            .seek(SeekFrom::Start(at))
            .map_err(|_| "compact_file_io")?;
        self.file
            .read_exact(&mut bytes)
            .map_err(|_| "compact_file_io")?;
        Ok(bytes)
    }

    fn replace_u64(&mut self, at: u64, value: u64) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(at))
            .map_err(|_| "compact_file_io")?;
        self.file
            .write_all(&value.to_le_bytes())
            .map_err(|_| "compact_file_io")
    }

    fn equal_keys(&mut self, left: Span, right: Span) -> Result<bool> {
        if left.end - left.start != right.end - right.start {
            return Ok(false);
        }
        let mut offset = 0;
        let mut a = [0; 8192];
        let mut b = [0; 8192];
        while offset < left.end - left.start {
            if self.cancelled.load(Ordering::Relaxed) {
                return Err("compact_cancelled");
            }
            let count = (left.end - left.start - offset).min(a.len() as u64) as usize;
            self.file
                .seek(SeekFrom::Start(left.start + offset))
                .map_err(|_| "compact_file_io")?;
            self.file
                .read_exact(&mut a[..count])
                .map_err(|_| "compact_file_io")?;
            self.file
                .seek(SeekFrom::Start(right.start + offset))
                .map_err(|_| "compact_file_io")?;
            self.file
                .read_exact(&mut b[..count])
                .map_err(|_| "compact_file_io")?;
            if a[..count] != b[..count] {
                return Ok(false);
            }
            offset += count as u64;
        }
        Ok(true)
    }

    // A compressed binary radix tree of SHA-256 digests. Internal bit indexes
    // strictly increase, so lookup takes at most 256 disk records, regardless
    // of object size. Equal digests still compare the complete decoded keys.
    pub(crate) fn insert_key(&mut self, root: &mut u64, digest: [u8; 32], key: Span) -> Result<()> {
        let leaf = |index: &mut Self, next: u64| -> Result<u64> {
            let mut record = [0; 64];
            record[..2].copy_from_slice(&u16::MAX.to_le_bytes());
            record[8..40].copy_from_slice(&digest);
            record[40..48].copy_from_slice(&key.start.to_le_bytes());
            record[48..56].copy_from_slice(&key.end.to_le_bytes());
            record[56..64].copy_from_slice(&next.to_le_bytes());
            index.append(&record)
        };
        if *root == 0 {
            *root = leaf(self, 0)?;
            return Ok(());
        }
        let mut at = *root;
        loop {
            if self.cancelled.load(Ordering::Relaxed) {
                return Err("compact_cancelled");
            }
            let record = self.read::<64>(at)?;
            let bit = u16::from_le_bytes(record[..2].try_into().unwrap());
            if bit == u16::MAX {
                let old: [u8; 32] = record[8..40].try_into().unwrap();
                if old == digest {
                    let mut current = Some(record);
                    let mut next_at = at;
                    while let Some(record) = current {
                        let old_key = Span {
                            start: u64_at(&record, 40),
                            end: u64_at(&record, 48),
                        };
                        if self.equal_keys(old_key, key)? {
                            return Err("duplicate_json_key");
                        }
                        let next = u64_at(&record, 56);
                        if next == 0 {
                            let new = leaf(self, 0)?;
                            self.replace_u64(next_at + 56, new)?;
                            return Ok(());
                        }
                        next_at = next;
                        current = Some(self.read::<64>(next)?);
                    }
                }
                let differing = old
                    .iter()
                    .zip(digest)
                    .enumerate()
                    .find_map(|(i, (&a, b))| {
                        (a != b).then(|| (i * 8 + (a ^ b).leading_zeros() as usize) as u16)
                    })
                    .ok_or("compact_file_io")?;
                let new_leaf = leaf(self, 0)?;
                // Find the parent edge before this bit, preserving radix order.
                let mut edge = None;
                let mut subtree = *root;
                loop {
                    let parent = self.read::<64>(subtree)?;
                    let parent_bit = u16::from_le_bytes(parent[..2].try_into().unwrap());
                    if parent_bit >= differing {
                        break;
                    }
                    let field = if digest_bit(&digest, parent_bit) {
                        16
                    } else {
                        8
                    };
                    edge = Some(subtree + field as u64);
                    subtree = u64_at(&parent, field);
                }
                let mut branch = [0; 64];
                branch[..2].copy_from_slice(&differing.to_le_bytes());
                let (zero, one) = if digest_bit(&digest, differing) {
                    (subtree, new_leaf)
                } else {
                    (new_leaf, subtree)
                };
                branch[8..16].copy_from_slice(&zero.to_le_bytes());
                branch[16..24].copy_from_slice(&one.to_le_bytes());
                let new_branch = self.append(&branch)?;
                if let Some(edge) = edge {
                    self.replace_u64(edge, new_branch)?;
                } else {
                    *root = new_branch;
                }
                return Ok(());
            }
            at = u64_at(&record, if digest_bit(&digest, bit) { 16 } else { 8 });
        }
    }

    pub(crate) fn append_record(&mut self, tail: &mut u64, bytes: &[u8]) -> Result<u64> {
        let mut header = [0; 16];
        header[8..].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        let at = self.append(&header)?;
        self.append(bytes)?;
        if *tail != 0 {
            self.replace_u64(*tail, at)?;
        }
        *tail = at;
        Ok(at)
    }

    pub(crate) fn record(&mut self, at: u64) -> Result<(Vec<u8>, u64)> {
        let header = self.read::<16>(at)?;
        let mut bytes = vec![
            0;
            u64_at(&header, 8)
                .try_into()
                .map_err(|_| "compact_file_io")?
        ];
        self.file
            .read_exact(&mut bytes)
            .map_err(|_| "compact_file_io")?;
        Ok((bytes, u64_at(&header, 0)))
    }

    pub(crate) fn append_span(&mut self, tail: &mut u64, span: Span) -> Result<u64> {
        let mut record = [0; 24];
        record[..8].copy_from_slice(&span.start.to_le_bytes());
        record[8..16].copy_from_slice(&span.end.to_le_bytes());
        let at = self.append(&record)?;
        if *tail != 0 {
            self.replace_u64(*tail + 16, at)?;
        }
        *tail = at;
        Ok(at)
    }

    pub(crate) fn span(&mut self, at: u64) -> Result<(Span, u64)> {
        let record = self.read::<24>(at)?;
        Ok((
            Span {
                start: u64_at(&record, 0),
                end: u64_at(&record, 8),
            },
            u64_at(&record, 16),
        ))
    }
}

struct ScratchBackend {
    scope: crate::archive::scratch_scope::ScratchScope,
    runtime: tokio::runtime::Handle,
    cancelled: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    _lease: tokio::sync::OwnedSemaphorePermit,
}
impl ScratchBackend {
    fn run<T>(
        &self,
        future: impl std::future::Future<Output = std::result::Result<T, crate::error::AppError>>,
    ) -> std::io::Result<T> {
        self.runtime.block_on(async {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancelled.load(Ordering::Acquire) {
                return Err(std::io::Error::other("compact_cancelled"));
            }
            tokio::select! {
                biased;
                _ = notified => Err(std::io::Error::other("compact_cancelled")),
                result = future => result.map_err(|_| std::io::Error::other("compact_scratch_io")),
            }
        })
    }
}
impl super::pager::PageBackend for ScratchBackend {
    fn read_page(&mut self, number: u64, bytes: &mut [u8]) -> std::io::Result<()> {
        self.run(self.scope.read_page(number, bytes))
    }
    fn write_page(&mut self, number: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.run(self.scope.write_page(number, bytes))
    }
}

fn u64_at(record: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(record[at..at + 8].try_into().unwrap())
}
fn digest_bit(digest: &[u8; 32], bit: u16) -> bool {
    digest[bit as usize / 8] & (1 << (7 - bit % 8)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[tokio::test]
    async fn large_decoded_keys_spill_with_a_fixed_metadata_lease() {
        use crate::{
            archive::{ArchiveStore, scratch_scope::ScratchScope},
            archive_staging::*,
            gateway_body::{
                carrier::{BodyCarrier, CarrierCapture},
                memory::ProxyMemoryBudget,
            },
        };
        let archive = ArchiveStore::memory_for_test();
        let memory = Arc::new(ProxyMemoryBudget::new(128 * 1024));
        let id = uuid::Uuid::new_v4();
        let key = ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(id),
            ArchiveStagingPurpose::Response,
            id,
        )
        .unwrap();
        let source = BodyCarrier::capture(
            futures_util::stream::empty::<std::result::Result<bytes::Bytes, std::io::Error>>(),
            CarrierCapture::new(memory.clone(), Some(archive.clone()), key),
        )
        .await
        .unwrap();
        let lease = source
            .reserve_metadata_workspace(super::super::pager::WORKSPACE_BYTES)
            .await
            .unwrap();
        let runtime = tokio::runtime::Handle::current();
        let tracked_memory = memory.clone();
        tokio::task::spawn_blocking(move || {
            let mut index = Index::scratch(
                ScratchScope::for_test(archive, key),
                lease,
                runtime,
                Arc::new(AtomicBool::new(false)),
                Arc::new(tokio::sync::Notify::new()),
            )
            .unwrap();
            let mut root = 0;
            for number in 0..8u8 {
                let key = vec![number; 128 * 1024];
                let start = index.append(&key).unwrap();
                index
                    .insert_key(
                        &mut root,
                        Sha256::digest(&key).into(),
                        Span {
                            start,
                            end: start + key.len() as u64,
                        },
                    )
                    .unwrap();
                assert_eq!(
                    tracked_memory.snapshot().0,
                    super::super::pager::WORKSPACE_BYTES
                );
            }
            let key = vec![3; 128 * 1024];
            let start = index.append(&key).unwrap();
            assert_eq!(
                index.insert_key(
                    &mut root,
                    Sha256::digest(&key).into(),
                    Span {
                        start,
                        end: start + key.len() as u64
                    }
                ),
                Err("duplicate_json_key")
            );
        })
        .await
        .unwrap();
        assert_eq!(memory.snapshot().0, 0);
    }

    #[test]
    fn ram_radix_preserves_all_keys_and_exact_digest_collision_comparison() {
        let mut index = Index::memory().unwrap();
        let mut root = 0;
        for number in 0..2048u32 {
            let key = number.to_le_bytes();
            let start = index.append(&key).unwrap();
            index
                .insert_key(
                    &mut root,
                    Sha256::digest(key).into(),
                    Span {
                        start,
                        end: start + 4,
                    },
                )
                .unwrap();
        }
        for number in [0u32, 2047, 17, 1024, 999] {
            let key = number.to_le_bytes();
            let start = index.append(&key).unwrap();
            assert_eq!(
                index.insert_key(
                    &mut root,
                    Sha256::digest(key).into(),
                    Span {
                        start,
                        end: start + 4
                    }
                ),
                Err("duplicate_json_key")
            );
        }
        let mut collision_root = 0;
        for key in [b"one".as_slice(), b"two".as_slice()] {
            let start = index.append(key).unwrap();
            index
                .insert_key(
                    &mut collision_root,
                    [7; 32],
                    Span {
                        start,
                        end: start + key.len() as u64,
                    },
                )
                .unwrap();
        }
        let start = index.append(b"two").unwrap();
        assert_eq!(
            index.insert_key(
                &mut collision_root,
                [7; 32],
                Span {
                    start,
                    end: start + 3
                }
            ),
            Err("duplicate_json_key")
        );
    }
}
