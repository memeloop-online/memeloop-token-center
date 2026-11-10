//! Disk-backed JSON syntax and span indexing shared by protocol projections.
//! Callers retain their own protocol validation and terminal evidence.
pub(crate) mod index;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) const CHUNK: usize = 64 * 1024;
pub(crate) type Result<T> = std::result::Result<T, &'static str>;

/// Select a finite set of protocol fields to keep in memory. Array elements,
/// raw fields and exact decoded-key equality are indexed on disk.
pub(crate) trait Projection: Copy {
    fn field(self, key: &str) -> Self;
    fn element(self) -> Self;
    fn retain_field(self, key: &str) -> bool;
    fn retain_elements(self) -> bool;
    fn retain_raw_fields(self) -> bool;
    fn retain_raw_field(self, key: &str) -> bool;
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub(crate) struct Span {
    pub(crate) start: u64,
    pub(crate) end: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum Kind {
    Null,
    Bool(bool),
    Number,
    String(bool),
    Object(Vec<(String, Node)>),
    Array(DiskNodes),
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Node {
    pub(crate) span: Span,
    pub(crate) kind: Kind,
    pub(crate) raw_fields: Option<DiskNodes>,
}

impl Node {
    pub(crate) fn get(&self, name: &str) -> Option<&Node> {
        match &self.kind {
            Kind::Object(fields) => fields.iter().find(|(key, _)| key == name).map(|(_, n)| n),
            _ => None,
        }
    }

    pub(crate) fn string(&self) -> bool {
        matches!(self.kind, Kind::String(_))
    }

    pub(crate) fn nonempty_string(&self) -> bool {
        matches!(self.kind, Kind::String(true))
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub(crate) struct DiskNodes {
    pub(crate) head: u64,
    pub(crate) count: usize,
}

pub(crate) struct Scanner {
    pub(crate) reader: BufReader<File>,
    pub(crate) index: Option<index::Index>,
    pub(crate) position: u64,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl Scanner {
    #[cfg(test)]
    pub(crate) fn new(file: File) -> Self {
        Self::with_index(file, tempfile::tempfile().unwrap()).unwrap()
    }

    pub(crate) fn with_index(file: File, index_file: File) -> Result<Self> {
        Ok(Self {
            reader: BufReader::with_capacity(CHUNK, file),
            index: Some(index::Index::new(index_file)?),
            position: 0,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    fn key(&mut self, root: &mut u64) -> Result<String> {
        let mut index = self.index.take().ok_or("compact_file_io")?;
        let start = index.end();
        let mut digest = Sha256::new();
        let mut small = Vec::new();
        let mut pending = Vec::new();
        let mut oversized = false;
        let result = self
            .string(|bytes| {
                digest.update(bytes);
                if pending.len() + bytes.len() > CHUNK && !pending.is_empty() {
                    index.append(&pending)?;
                    pending.clear();
                }
                if bytes.len() == CHUNK {
                    index.append(bytes)?;
                } else {
                    pending.extend_from_slice(bytes);
                }
                if !oversized && small.len() + bytes.len() <= 64 {
                    small.extend_from_slice(bytes);
                } else {
                    oversized = true;
                    small.clear();
                }
                Ok(())
            })
            .and_then(|_| {
                if !pending.is_empty() {
                    index.append(&pending)?;
                }
                let key = Span {
                    start,
                    end: index.end(),
                };
                index.insert_key(root, digest.finalize().into(), key)
            });
        self.index = Some(index);
        result?;
        String::from_utf8(small).map_err(|_| "invalid_json")
    }

    pub(crate) fn disk_node(&mut self, at: &mut u64) -> Result<Option<Node>> {
        self.check_cancelled()?;
        if *at == 0 {
            return Ok(None);
        }
        let (bytes, next) = self.index.as_mut().ok_or("compact_file_io")?.record(*at)?;
        *at = next;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "compact_file_io")
    }

    pub(crate) fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err("compact_cancelled")
        } else {
            Ok(())
        }
    }

    pub(crate) fn seek(&mut self, position: u64) -> Result<()> {
        self.check_cancelled()?;
        self.reader
            .seek(SeekFrom::Start(position))
            .map_err(|_| "compact_file_io")?;
        self.position = position;
        Ok(())
    }

    pub(crate) fn peek(&mut self) -> Result<Option<u8>> {
        self.check_cancelled()?;
        Ok(self
            .reader
            .fill_buf()
            .map_err(|_| "compact_file_io")?
            .first()
            .copied())
    }

    pub(crate) fn byte(&mut self) -> Result<u8> {
        let byte = self.peek()?.ok_or("invalid_json")?;
        self.reader.consume(1);
        self.position += 1;
        Ok(byte)
    }

    pub(crate) fn expect(&mut self, byte: u8) -> Result<()> {
        if self.byte()? == byte {
            Ok(())
        } else {
            Err("invalid_json")
        }
    }

    pub(crate) fn whitespace(&mut self) -> Result<()> {
        while matches!(self.peek()?, Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.byte()?;
        }
        Ok(())
    }

    fn hex(&mut self) -> Result<u16> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = match self.byte()? {
                b @ b'0'..=b'9' => b - b'0',
                b @ b'a'..=b'f' => b - b'a' + 10,
                b @ b'A'..=b'F' => b - b'A' + 10,
                _ => return Err("invalid_json"),
            };
            value = value * 16 + u16::from(digit);
        }
        Ok(value)
    }

    /// Decode one JSON string into a streaming sink, including surrogate pairs.
    pub(crate) fn string(&mut self, mut sink: impl FnMut(&[u8]) -> Result<()>) -> Result<bool> {
        self.expect(b'"')?;
        let mut nonempty = false;
        loop {
            self.check_cancelled()?;
            // Most provider blobs and text are ASCII. Process a buffer slice at a time
            // rather than calling the base64 encoder once per character.
            let available = self.reader.fill_buf().map_err(|_| "compact_file_io")?;
            let run = available
                .iter()
                .take_while(|&&b| (32..=127).contains(&b) && b != b'"' && b != b'\\')
                .count();
            if run != 0 {
                sink(&available[..run])?;
                self.reader.consume(run);
                self.position += run as u64;
                nonempty = true;
                continue;
            }
            let byte = self.byte()?;
            match byte {
                b'"' => return Ok(nonempty),
                0..=31 => return Err("invalid_json"),
                b'\\' => {
                    let escaped = self.byte()?;
                    let ascii = match escaped {
                        b'"' | b'\\' | b'/' => Some(escaped),
                        b'b' => Some(8),
                        b'f' => Some(12),
                        b'n' => Some(b'\n'),
                        b'r' => Some(b'\r'),
                        b't' => Some(b'\t'),
                        b'u' => None,
                        _ => return Err("invalid_json"),
                    };
                    if let Some(ascii) = ascii {
                        sink(&[ascii])?;
                    } else {
                        let first = self.hex()?;
                        let scalar = if (0xd800..=0xdbff).contains(&first) {
                            self.expect(b'\\')?;
                            self.expect(b'u')?;
                            let second = self.hex()?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                return Err("invalid_json");
                            }
                            0x10000
                                + ((u32::from(first) - 0xd800) << 10)
                                + (u32::from(second) - 0xdc00)
                        } else {
                            u32::from(first)
                        };
                        let character = char::from_u32(scalar).ok_or("invalid_json")?;
                        let mut bytes = [0; 4];
                        sink(character.encode_utf8(&mut bytes).as_bytes())?;
                    }
                }
                32..=127 => sink(&[byte])?,
                _ => {
                    let length = match byte {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err("invalid_json"),
                    };
                    let mut bytes = [0; 4];
                    bytes[0] = byte;
                    for next in bytes.iter_mut().take(length).skip(1) {
                        *next = self.byte()?;
                    }
                    std::str::from_utf8(&bytes[..length]).map_err(|_| "invalid_json")?;
                    sink(&bytes[..length])?;
                }
            }
            nonempty = true;
        }
    }

    pub(crate) fn value<P: Projection>(&mut self, capture: P, depth: usize) -> Result<Node> {
        // Match serde_json's nesting protection; this is independent of body size.
        if depth > 128 {
            return Err("invalid_json");
        }
        self.whitespace()?;
        let start = self.position;
        let mut raw_fields = None;
        let kind = match self.peek()?.ok_or("invalid_json")? {
            b'"' => Kind::String(self.string(|_| Ok(()))?),
            b'{' => {
                self.byte()?;
                self.whitespace()?;
                let mut fields = Vec::new();
                let mut keys = 0u64;
                let mut raw = DiskNodes { head: 0, count: 0 };
                let mut raw_tail = 0;
                if self.peek()? != Some(b'}') {
                    loop {
                        let field_start = self.position;
                        let key = self.key(&mut keys)?;
                        self.whitespace()?;
                        self.expect(b':')?;
                        let child_capture = capture.field(&key);
                        let child = self.value(child_capture, depth + 1)?;
                        if capture.retain_raw_field(&key) {
                            let span = Span {
                                start: field_start,
                                end: child.span.end,
                            };
                            let at = self
                                .index
                                .as_mut()
                                .ok_or("compact_file_io")?
                                .append_span(&mut raw_tail, span)?;
                            if raw.head == 0 {
                                raw.head = at;
                            }
                            raw.count += 1;
                        }

                        if capture.retain_field(&key) {
                            fields.push((key, child));
                        }
                        self.whitespace()?;
                        match self.byte()? {
                            b'}' => break,
                            b',' => self.whitespace()?,
                            _ => return Err("invalid_json"),
                        }
                    }
                } else {
                    self.byte()?;
                }
                if capture.retain_raw_fields() {
                    raw_fields = Some(raw);
                }
                Kind::Object(fields)
            }
            b'[' => {
                self.byte()?;
                self.whitespace()?;
                let mut items = DiskNodes { head: 0, count: 0 };
                let mut tail = 0;
                if self.peek()? != Some(b']') {
                    loop {
                        let child_capture = capture.element();
                        let child = self.value(child_capture, depth + 1)?;
                        if capture.retain_elements() {
                            let bytes =
                                serde_json::to_vec(&child).map_err(|_| "compact_file_io")?;
                            let at = self
                                .index
                                .as_mut()
                                .ok_or("compact_file_io")?
                                .append_record(&mut tail, &bytes)?;
                            if items.head == 0 {
                                items.head = at;
                            }
                            items.count += 1;
                        }
                        self.whitespace()?;
                        match self.byte()? {
                            b']' => break,
                            b',' => self.whitespace()?,
                            _ => return Err("invalid_json"),
                        }
                    }
                } else {
                    self.byte()?;
                }
                Kind::Array(items)
            }
            b'n' => {
                self.literal(b"null")?;
                Kind::Null
            }
            b't' => {
                self.literal(b"true")?;
                Kind::Bool(true)
            }
            b'f' => {
                self.literal(b"false")?;
                Kind::Bool(false)
            }
            b'-' | b'0'..=b'9' => {
                self.number()?;
                Kind::Number
            }
            _ => return Err("invalid_json"),
        };
        Ok(Node {
            span: Span {
                start,
                end: self.position,
            },
            kind,
            raw_fields,
        })
    }

    pub(crate) fn literal(&mut self, literal: &[u8]) -> Result<()> {
        for &byte in literal {
            self.expect(byte)?;
        }
        Ok(())
    }

    fn digits(&mut self) -> Result<()> {
        if !matches!(self.peek()?, Some(b'0'..=b'9')) {
            return Err("invalid_json");
        }
        while matches!(self.peek()?, Some(b'0'..=b'9')) {
            self.byte()?;
        }
        Ok(())
    }

    fn number(&mut self) -> Result<()> {
        let start = self.position;
        if self.peek()? == Some(b'-') {
            self.byte()?;
        }
        match self.peek()? {
            Some(b'0') => {
                self.byte()?;
            }
            Some(b'1'..=b'9') => self.digits()?,
            _ => return Err("invalid_json"),
        }
        if self.peek()? == Some(b'.') {
            self.byte()?;
            self.digits()?;
        }
        if matches!(self.peek()?, Some(b'e' | b'E')) {
            self.byte()?;
            if matches!(self.peek()?, Some(b'+' | b'-')) {
                self.byte()?;
            }
            self.digits()?;
        }
        let end = self.position;
        self.seek(start)?;
        struct NumberReader<'a> {
            source: std::io::Take<&'a mut BufReader<File>>,
            cancelled: Arc<AtomicBool>,
        }
        impl Read for NumberReader<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if self.cancelled.load(Ordering::Relaxed) {
                    return Err(std::io::Error::other("compact cancelled"));
                }
                self.source.read(bytes)
            }
        }
        let reader = NumberReader {
            source: (&mut self.reader).take(end - start),
            cancelled: self.cancelled.clone(),
        };
        // Match the existing unique-JSON parser's finite number semantics
        // without storing an arbitrarily long number spelling.
        let number = serde_json::from_reader::<_, serde_json::Number>(reader);
        self.check_cancelled()?;
        number.map_err(|_| "invalid_json")?;
        self.seek(end)?;
        Ok(())
    }

    pub(crate) fn equals(&mut self, node: &Node, expected: &str) -> Result<bool> {
        if !node.string() {
            return Ok(false);
        }
        self.seek(node.span.start)?;
        let mut offset = 0usize;
        let mut equal = true;
        self.string(|bytes| {
            if expected
                .as_bytes()
                .get(offset..offset.saturating_add(bytes.len()))
                != Some(bytes)
            {
                equal = false;
            }
            offset = offset.saturating_add(bytes.len());
            Ok(())
        })?;
        Ok(equal && offset == expected.len())
    }

    /// Copy raw JSON while removing only whitespace outside strings (SSE data is one line).
    pub(crate) fn copy(&mut self, span: Span, out: &mut impl Write) -> Result<()> {
        self.seek(span.start)?;
        let mut remaining = span.end - span.start;
        let mut inside = false;
        let mut escaped = false;
        let mut buffer = [0; CHUNK];
        while remaining > 0 {
            self.check_cancelled()?;
            let length = remaining.min(CHUNK as u64) as usize;
            self.reader
                .read_exact(&mut buffer[..length])
                .map_err(|_| "compact_file_io")?;
            self.position += length as u64;
            remaining -= length as u64;
            let mut kept = 0;
            for index in 0..length {
                let byte = buffer[index];
                if inside {
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        inside = false;
                    }
                } else if byte == b'"' {
                    inside = true;
                } else if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                    continue;
                }
                buffer[kept] = byte;
                kept += 1;
            }
            write(out, &buffer[..kept])?;
        }
        Ok(())
    }
}

fn write(out: &mut impl Write, bytes: &[u8]) -> Result<()> {
    out.write_all(bytes).map_err(|_| "compact_file_io")
}
