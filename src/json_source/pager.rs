//! Fixed resident scratch pages. The backend owns the prepaid workspace and
//! typed scratch scope; no page directory or payload grows in this cache.
use std::io::{self, Read, Seek, SeekFrom, Write};

pub(crate) const PAGE_BYTES: usize = 16 * 1024;
const RESIDENT_PAGES: usize = 2;
// Two resident pages and one read/write publication workspace. Metadata fits
// inside the remaining budget unit rather than borrowing provider body credit.
pub(crate) const WORKSPACE_BYTES: usize = PAGE_BYTES * (RESIDENT_PAGES + 2);

pub(crate) trait PageBackend: Send {
    fn read_page(&mut self, number: u64, bytes: &mut [u8]) -> io::Result<()>;
    fn write_page(&mut self, number: u64, bytes: &[u8]) -> io::Result<()>;
}

struct Frame {
    number: Option<u64>,
    bytes: Vec<u8>,
    dirty: bool,
    used: u64,
}

pub(crate) struct Pager {
    frames: [Frame; RESIDENT_PAGES],
    position: u64,
    length: u64,
    published_pages: u64,
    clock: u64,
    backend: Box<dyn PageBackend>,
}

impl Pager {
    // The caller must acquire WORKSPACE_BYTES before constructing the backend;
    // its owned reservation lives until after these frames have been dropped.
    pub(crate) fn new(backend: Box<dyn PageBackend>) -> Self {
        Self {
            backend,
            frames: std::array::from_fn(|_| Frame {
                number: None,
                bytes: vec![0; PAGE_BYTES],
                dirty: false,
                used: 0,
            }),
            position: 0,
            length: 0,
            published_pages: 0,
            clock: 0,
        }
    }

    fn frame(&mut self, number: u64) -> io::Result<usize> {
        self.clock = self.clock.saturating_add(1);
        if let Some(index) = self
            .frames
            .iter()
            .position(|frame| frame.number == Some(number))
        {
            self.frames[index].used = self.clock;
            return Ok(index);
        }
        let index = self
            .frames
            .iter()
            .position(|frame| frame.number.is_none())
            .unwrap_or_else(|| {
                self.frames
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, frame)| frame.used)
                    .unwrap()
                    .0
            });
        let frame = &mut self.frames[index];
        if frame.dirty {
            // Failed or ambiguous publication keeps the only dirty bytes and
            // their reservation intact. The backend resolves uncertain writes.
            self.backend
                .write_page(frame.number.unwrap(), &frame.bytes)?;
            self.published_pages = self.published_pages.max(frame.number.unwrap() + 1);
            frame.dirty = false;
        }
        frame.number = None;
        if number < self.published_pages {
            self.backend.read_page(number, &mut frame.bytes)?;
        } else {
            frame.bytes.fill(0);
        }
        frame.number = Some(number);
        frame.used = self.clock;
        Ok(index)
    }
}

impl Read for Pager {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = (self.length.saturating_sub(self.position))
            .min(output.len() as u64)
            .min((PAGE_BYTES - self.position as usize % PAGE_BYTES) as u64)
            as usize;
        if count == 0 {
            return Ok(0);
        }
        let index = self.frame(self.position / PAGE_BYTES as u64)?;
        let start = self.position as usize % PAGE_BYTES;
        output[..count].copy_from_slice(&self.frames[index].bytes[start..start + count]);
        self.position += count as u64;
        Ok(count)
    }
}
impl Write for Pager {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        // Index writes append or replace existing records; sparse writes are
        // unnecessary and would require an unbounded absent-page directory.
        if self.position > self.length {
            return Err(io::Error::other("compact_index_sparse_write"));
        }
        let count = input
            .len()
            .min(PAGE_BYTES - self.position as usize % PAGE_BYTES);
        let index = self.frame(self.position / PAGE_BYTES as u64)?;
        let start = self.position as usize % PAGE_BYTES;
        self.frames[index].bytes[start..start + count].copy_from_slice(&input[..count]);
        self.frames[index].dirty = true;
        self.position += count as u64;
        self.length = self.length.max(self.position);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        for frame in &mut self.frames {
            if frame.dirty {
                self.backend
                    .write_page(frame.number.unwrap(), &frame.bytes)?;
                self.published_pages = self.published_pages.max(frame.number.unwrap() + 1);
                frame.dirty = false;
            }
        }
        Ok(())
    }
}
impl Seek for Pager {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(at) => i128::from(at),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.length) + i128::from(delta),
        };
        self.position = u64::try_from(next).map_err(|_| io::Error::other("compact_index_seek"))?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    struct TestBackend {
        pages: Arc<Mutex<BTreeMap<u64, Vec<u8>>>>,
        fail: Arc<AtomicBool>,
    }
    impl PageBackend for TestBackend {
        fn read_page(&mut self, number: u64, bytes: &mut [u8]) -> io::Result<()> {
            bytes.copy_from_slice(
                self.pages
                    .lock()
                    .unwrap()
                    .get(&number)
                    .ok_or_else(|| io::Error::other("missing_page"))?,
            );
            Ok(())
        }
        fn write_page(&mut self, number: u64, bytes: &[u8]) -> io::Result<()> {
            if self.fail.load(Ordering::Acquire) {
                return Err(io::Error::other("pending_publication"));
            }
            self.pages.lock().unwrap().insert(number, bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn bounded_cache_reloads_evicted_pages_and_preserves_dirty_page_on_failure() {
        let fail = Arc::new(AtomicBool::new(false));
        let pages = Arc::new(Mutex::new(BTreeMap::new()));
        let mut pager = Pager::new(Box::new(TestBackend {
            pages: pages.clone(),
            fail: fail.clone(),
        }));
        let mut expected = Vec::new();
        for number in 0..19u8 {
            let bytes = vec![number; PAGE_BYTES];
            pager.write_all(&bytes).unwrap();
            expected.extend_from_slice(&bytes);
        }
        pager.seek(SeekFrom::Start(7)).unwrap();
        pager.write_all(b"changed").unwrap();
        expected[7..14].copy_from_slice(b"changed");
        fail.store(true, Ordering::Release);
        assert!(pager.flush().is_err());
        pager.seek(SeekFrom::Start(7)).unwrap();
        let mut dirty = [0; 7];
        pager.read_exact(&mut dirty).unwrap();
        assert_eq!(&dirty, b"changed");
        fail.store(false, Ordering::Release);
        pager.flush().unwrap();
        pager.rewind().unwrap();
        let mut actual = Vec::new();
        pager.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(pager.frames.len(), RESIDENT_PAGES);
        assert_eq!(pages.lock().unwrap().len(), 19);
    }
}
