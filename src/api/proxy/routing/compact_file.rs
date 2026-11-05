//! File-backed compact translation. Payload strings are never materialized in memory.
mod index;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const CHUNK: usize = 64 * 1024;
type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Span {
    start: u64,
    end: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum Kind {
    Null,
    Bool(bool),
    Number,
    String(bool),
    Object(Vec<(String, Node)>),
    Array(DiskNodes),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Node {
    span: Span,
    kind: Kind,
    raw_fields: Option<DiskNodes>,
}

impl Node {
    fn get(&self, name: &str) -> Option<&Node> {
        match &self.kind {
            Kind::Object(fields) => fields.iter().find(|(key, _)| key == name).map(|(_, n)| n),
            _ => None,
        }
    }

    fn string(&self) -> bool {
        matches!(self.kind, Kind::String(_))
    }

    fn nonempty_string(&self) -> bool {
        matches!(self.kind, Kind::String(true))
    }
}

#[derive(Clone, Copy)]
enum Capture {
    Root,
    Output,
    Item,
    Content,
    Part,
    Usage,
    UsageDetails,
    UsageEnvelope,
    Skip,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct DiskNodes {
    head: u64,
    count: usize,
}

fn recognized_key(capture: Capture, key: &str) -> bool {
    match capture {
        Capture::Root => matches!(
            key,
            "id" | "error"
                | "status"
                | "incomplete_details"
                | "output"
                | "usage"
                | "message"
                | "response"
                | "service_tier"
        ),
        Capture::Item => matches!(
            key,
            "id" | "type" | "status" | "incomplete_details" | "encrypted_content" | "content"
        ),
        Capture::Part => matches!(key, "type" | "text"),
        Capture::UsageEnvelope => matches!(key, "usage" | "service_tier"),
        Capture::Usage | Capture::UsageDetails => matches!(
            key,
            "service_tier"
                | "input_tokens"
                | "prompt_tokens"
                | "output_tokens"
                | "completion_tokens"
                | "prompt_cache_hit_tokens"
                | "prompt_cache_miss_tokens"
                | "input_tokens_details"
                | "prompt_tokens_details"
                | "cache_read_input_tokens"
                | "cache_creation_input_tokens"
                | "cache_creation"
                | "cached_tokens"
                | "ephemeral_5m_input_tokens"
                | "ephemeral_1h_input_tokens"
                | "cache_read_observed"
                | "cache_usage_complete"
        ),
        _ => false,
    }
}

struct Scanner {
    reader: BufReader<File>,
    index: Option<index::Index>,
    position: u64,
    cancelled: Arc<AtomicBool>,
}

impl Scanner {
    #[cfg(test)]
    fn new(file: File) -> Self {
        Self::with_index(file, tempfile::tempfile().unwrap()).unwrap()
    }

    fn with_index(file: File, index_file: File) -> Result<Self> {
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
        let mut oversized = false;
        let result = self
            .string(|bytes| {
                digest.update(bytes);
                index.append(bytes)?;
                if !oversized && small.len() + bytes.len() <= 64 {
                    small.extend_from_slice(bytes);
                } else {
                    oversized = true;
                    small.clear();
                }
                Ok(())
            })
            .and_then(|_| {
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

    fn disk_node(&mut self, at: &mut u64) -> Result<Option<Node>> {
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

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err("compact_cancelled")
        } else {
            Ok(())
        }
    }

    fn seek(&mut self, position: u64) -> Result<()> {
        self.check_cancelled()?;
        self.reader
            .seek(SeekFrom::Start(position))
            .map_err(|_| "compact_file_io")?;
        self.position = position;
        Ok(())
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        self.check_cancelled()?;
        Ok(self
            .reader
            .fill_buf()
            .map_err(|_| "compact_file_io")?
            .first()
            .copied())
    }

    fn byte(&mut self) -> Result<u8> {
        let byte = self.peek()?.ok_or("invalid_json")?;
        self.reader.consume(1);
        self.position += 1;
        Ok(byte)
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.byte()? == byte {
            Ok(())
        } else {
            Err("invalid_json")
        }
    }

    fn whitespace(&mut self) -> Result<()> {
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
    fn string(&mut self, mut sink: impl FnMut(&[u8]) -> Result<()>) -> Result<bool> {
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

    fn value(&mut self, capture: Capture, depth: usize) -> Result<Node> {
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
                        let child_capture = match (capture, key.as_str()) {
                            (Capture::Root, "output") => Capture::Output,
                            (Capture::Root | Capture::UsageEnvelope, "usage") => Capture::Usage,
                            (Capture::Root, "message" | "response") => Capture::UsageEnvelope,
                            (
                                Capture::Usage,
                                "input_tokens_details" | "prompt_tokens_details" | "cache_creation",
                            ) => Capture::UsageDetails,
                            (Capture::Item, "content") => Capture::Content,
                            _ => Capture::Skip,
                        };
                        let child = self.value(child_capture, depth + 1)?;
                        if matches!(capture, Capture::Root)
                            && !matches!(
                                key.as_str(),
                                "id" | "object" | "status" | "error" | "output"
                            )
                        {
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

                        if recognized_key(capture, &key)
                            && matches!(
                                capture,
                                Capture::Root
                                    | Capture::Item
                                    | Capture::Part
                                    | Capture::Usage
                                    | Capture::UsageDetails
                                    | Capture::UsageEnvelope
                            )
                        {
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
                if matches!(capture, Capture::Root) {
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
                        let child_capture = match capture {
                            Capture::Output => Capture::Item,
                            Capture::Content => Capture::Part,
                            _ => Capture::Skip,
                        };
                        let child = self.value(child_capture, depth + 1)?;
                        if matches!(capture, Capture::Output | Capture::Content) {
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

    fn literal(&mut self, literal: &[u8]) -> Result<()> {
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

    fn equals(&mut self, node: &Node, expected: &str) -> Result<bool> {
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

    fn completed(&mut self, node: &Node) -> Result<()> {
        if node
            .get("incomplete_details")
            .is_some_and(|n| !matches!(n.kind, Kind::Null))
        {
            return Err("provider_not_completed");
        }
        if let Some(status) = node.get("status")
            && !matches!(status.kind, Kind::Null)
            && !self.equals(status, "completed")?
        {
            return Err("provider_not_completed");
        }
        Ok(())
    }

    /// Copy raw JSON while removing only whitespace outside strings (SSE data is one line).
    fn copy(&mut self, span: Span, out: &mut impl Write) -> Result<()> {
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

fn legacy_text(scanner: &mut Scanner, item: &Node) -> Result<bool> {
    let Some(content) = item.get("content") else {
        return Ok(false);
    };
    if content.string() {
        return Ok(content.nonempty_string());
    }
    let Kind::Array(parts) = &content.kind else {
        return Ok(false);
    };
    let mut at = parts.head;
    let mut nonempty = false;
    while let Some(part) = scanner.disk_node(&mut at)? {
        if let Some(kind) = part.get("type")
            && kind.string()
            && !scanner.equals(kind, "output_text")?
            && !scanner.equals(kind, "input_text")?
            && !scanner.equals(kind, "text")?
        {
            return Ok(false);
        }
        let Some(text) = part.get("text").filter(|n| n.string()) else {
            return Ok(false);
        };
        nonempty |= text.nonempty_string();
    }
    Ok(nonempty)
}

fn emit_item(
    scanner: &mut Scanner,
    out: &mut impl Write,
    root: &Node,
    item: &Node,
    translate_text: bool,
    index: usize,
    count: usize,
) -> Result<()> {
    if !translate_text {
        return scanner.copy(item.span, out);
    }
    write(out, br#"{"id":"cmp_"#)?;
    if let Some(id) = root.get("id").filter(|n| n.string()) {
        // Copy the interior verbatim: JSON escapes remain escapes, even for very large IDs.
        scanner.seek(id.span.start + 1)?;
        let mut remaining = id.span.end - id.span.start - 2;
        let mut buffer = [0; CHUNK];
        while remaining > 0 {
            scanner.check_cancelled()?;
            let length = remaining.min(CHUNK as u64) as usize;
            scanner
                .reader
                .read_exact(&mut buffer[..length])
                .map_err(|_| "compact_file_io")?;
            write(out, &buffer[..length])?;
            remaining -= length as u64;
        }
    } else {
        write(out, b"compact")?;
    }
    if count != 1 {
        write!(out, "_{index}").map_err(|_| "compact_file_io")?;
    }
    write(
        out,
        br#"","type":"compaction","encrypted_content":"mtc-compact-v1."#,
    )?;
    {
        let mut encoder = base64::write::EncoderWriter::new(
            &mut *out,
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        );
        // Keep one encoder across all parts: base64 boundaries follow concatenated UTF-8.
        let content = item.get("content").ok_or("output_item_malformed")?;
        if content.string() {
            scanner.seek(content.span.start)?;
            scanner.string(|bytes| write(&mut encoder, bytes))?;
        } else if let Kind::Array(parts) = &content.kind {
            let mut at = parts.head;
            while let Some(part) = scanner.disk_node(&mut at)? {
                let text = part.get("text").ok_or("output_item_malformed")?;
                scanner.seek(text.span.start)?;
                scanner.string(|bytes| write(&mut encoder, bytes))?;
            }
        }
        encoder.finish().map_err(|_| "compact_file_io")?;
    }
    write(out, br#""}"#)
}

struct ResponseLayout {
    status: Span,
    output_start: u64,
    items: DiskNodes,
    end: u64,
}

fn position(out: &mut (impl Write + Seek)) -> Result<u64> {
    out.stream_position().map_err(|_| "compact_file_io")
}

fn emit_response(
    scanner: &mut Scanner,
    out: &mut (impl Write + Seek),
    root: &Node,
    items: DiskNodes,
    native: bool,
) -> Result<ResponseLayout> {
    write(out, b"{")?;
    let mut field = root.raw_fields.map(|fields| fields.head).unwrap_or(0);
    while field != 0 {
        scanner.check_cancelled()?;
        let (span, next) = scanner
            .index
            .as_mut()
            .ok_or("compact_file_io")?
            .span(field)?;
        scanner.copy(span, out)?;
        write(out, b",")?;
        field = next;
    }
    write(out, br#""id":"#)?;
    if let Some(id) = root.get("id").filter(|n| n.string()) {
        scanner.copy(id.span, out)?;
    } else {
        write(out, br#""resp_compact""#)?;
    }
    write(out, br#","object":"response","error":null,"status":"#)?;
    let status_start = position(out)?;
    write(out, br#""completed""#)?;
    let status = Span {
        start: status_start,
        end: position(out)?,
    };
    write(out, br#","output":["#)?;
    let output_start = position(out)?;
    let mut item_spans = DiskNodes { head: 0, count: 0 };
    let mut span_tail = 0;
    let mut at = items.head;
    let mut index = 0;
    while let Some(item) = scanner.disk_node(&mut at)? {
        if index != 0 {
            write(out, b",")?;
        }
        let start = position(out)?;
        let translate_text = !native
            && scanner.equals(item.get("type").ok_or("output_item_malformed")?, "message")?
            && legacy_text(scanner, &item)?;
        emit_item(
            scanner,
            out,
            root,
            &item,
            translate_text,
            index,
            items.count,
        )?;
        let span = Span {
            start,
            end: position(out)?,
        };
        let record = scanner
            .index
            .as_mut()
            .ok_or("compact_file_io")?
            .append_span(&mut span_tail, span)?;
        if item_spans.head == 0 {
            item_spans.head = record;
        }
        item_spans.count += 1;
        index += 1;
    }
    write(out, b"]}")?;
    Ok(ResponseLayout {
        status,
        output_start,
        items: item_spans,
        end: position(out)?,
    })
}

/// Host evidence, constructed only after the complete compact response has been
/// validated and the corresponding output file has been written successfully.
/// It cannot be supplied through HTTP headers or provider JSON fields.
#[derive(Clone)]
pub(in crate::api::proxy) struct ValidatedCompact {
    usage: Option<crate::model::TokenUsage>,
    usage_invalid: bool,
    response_id: Option<String>,
    body_bytes: u64,
    first_billable_byte_end: u64,
}

impl ValidatedCompact {
    pub(in crate::api::proxy) fn summary(
        &self,
    ) -> crate::api::proxy::sse_capture::ResponsesSseSummary {
        crate::api::proxy::sse_capture::ResponsesSseSummary {
            outcome: crate::api::proxy::sse_capture::ResponsesSseOutcome::Completed {
                response_id: self.response_id.clone(),
            },
            supplier_rate_limited: false,
            usage: self.usage.clone(),
            usage_invalid: self.usage_invalid,
            observed_protocol_invalid: false,
            independently_observed_protocol_invalid: false,
            protocol_invalid: false,
        }
    }

    pub(in crate::api::proxy) fn first_billable_byte_end(&self) -> u64 {
        self.first_billable_byte_end
    }

    pub(in crate::api::proxy) fn expected_body_bytes(&self) -> u64 {
        self.body_bytes
    }
}

pub(super) struct TranslatedCompact {
    pub(super) file: File,
    pub(super) pieces: File,
    pub(super) physical_body_bytes: u64,
    pub(super) evidence: ValidatedCompact,
}

pub(super) enum CompactPiece {
    Range(std::ops::Range<u64>),
    Bytes(Vec<u8>),
}

fn response_pieces(
    layout: ResponseLayout,
    sse: bool,
    index: &mut index::Index,
    mut plan: File,
    cancelled: &AtomicBool,
) -> Result<(File, u64, u64)> {
    use CompactPiece::{Bytes, Range};
    plan.set_len(0).map_err(|_| "compact_file_io")?;
    plan.seek(SeekFrom::Start(0))
        .map_err(|_| "compact_file_io")?;
    let mut out = BufWriter::with_capacity(CHUNK, &mut plan);
    let mut total = 0u64;
    let mut emit = |piece: CompactPiece| -> Result<()> {
        if cancelled.load(Ordering::Relaxed) {
            return Err("compact_cancelled");
        }
        let length = match piece {
            Range(range) => {
                write(&mut out, &[0])?;
                write(&mut out, &range.start.to_le_bytes())?;
                write(&mut out, &range.end.to_le_bytes())?;
                range.end - range.start
            }
            Bytes(bytes) => {
                write(&mut out, &[1])?;
                write(&mut out, &(bytes.len() as u64).to_le_bytes())?;
                write(&mut out, &bytes)?;
                bytes.len() as u64
            }
        };
        total = total.checked_add(length).ok_or("compact_file_io")?;
        Ok(())
    };
    let mut first_billable = 0;
    if !sse {
        emit(Range(0..layout.end))?;
    } else {
        emit(Bytes(
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":".to_vec(),
        ))?;
        emit(Range(0..layout.status.start))?;
        emit(Bytes(br#""in_progress""#.to_vec()))?;
        emit(Range(layout.status.end..layout.output_start))?;
        emit(Bytes(b"]}}\n\n".to_vec()))?;
        let mut at = layout.items.head;
        let mut item_index = 0usize;
        // Track the boundary without borrowing the closure's running total.
        let mut added_lengths = 0u64;
        while at != 0 {
            let (item, next) = index.span(at)?;
            for event in ["response.output_item.added", "response.output_item.done"] {
                let prefix = format!("event: {event}\ndata: {{\"type\":\"{event}\",\"output_index\":{item_index},\"item\":").into_bytes();
                if item_index == 0 && event == "response.output_item.added" {
                    added_lengths = prefix.len() as u64 + item.end - item.start + 3;
                }
                emit(Bytes(prefix))?;
                emit(Range(item.start..item.end))?;
                emit(Bytes(b"}\n\n".to_vec()))?;
            }
            at = next;
            item_index += 1;
        }
        if layout.items.count != 0 {
            first_billable =
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":".len()
                    as u64
                    + layout.status.start
                    + br#""in_progress""#.len() as u64
                    + layout.output_start
                    - layout.status.end
                    + b"]}}\n\n".len() as u64
                    + added_lengths;
        }
        emit(Bytes(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":"
                .to_vec(),
        ))?;
        emit(Range(0..layout.end))?;
        emit(Bytes(b"}\n\n".to_vec()))?;
    }
    out.flush().map_err(|_| "compact_file_io")?;
    drop(out);
    plan.seek(SeekFrom::Start(0))
        .map_err(|_| "compact_file_io")?;
    Ok((
        plan,
        total,
        if first_billable == 0 {
            total
        } else {
            first_billable
        },
    ))
}

#[cfg(test)]
fn read_piece(input: &mut impl Read) -> Result<Option<CompactPiece>> {
    let mut tag = [0];
    if input.read(&mut tag).map_err(|_| "compact_file_io")? == 0 {
        return Ok(None);
    }
    let mut word = [0; 8];
    input.read_exact(&mut word).map_err(|_| "compact_file_io")?;
    let first = u64::from_le_bytes(word);
    match tag[0] {
        0 => {
            input.read_exact(&mut word).map_err(|_| "compact_file_io")?;
            Ok(Some(CompactPiece::Range(first..u64::from_le_bytes(word))))
        }
        1 => {
            let mut buffer = [0; 256];
            let length = usize::try_from(first).map_err(|_| "compact_file_io")?;
            let bytes = buffer.get_mut(..length).ok_or("compact_file_io")?;
            input.read_exact(bytes).map_err(|_| "compact_file_io")?;
            Ok(Some(CompactPiece::Bytes(bytes.to_vec())))
        }
        _ => Err("compact_file_io"),
    }
}

fn small_string(scanner: &mut Scanner, node: &Node, maximum: usize) -> Result<Option<String>> {
    if !node.string() {
        return Ok(None);
    }
    scanner.seek(node.span.start)?;
    let mut value = Vec::new();
    let mut overflowed = false;
    scanner.string(|bytes| {
        if !overflowed && value.len().saturating_add(bytes.len()) <= maximum {
            value.extend_from_slice(bytes);
        } else {
            overflowed = true;
            value.clear();
        }
        Ok(())
    })?;
    Ok((!overflowed).then(|| String::from_utf8(value).expect("scanner validates UTF-8")))
}

fn response_identity(scanner: &mut Scanner, node: &Node) -> Result<Option<String>> {
    scanner.seek(node.span.start)?;
    let mut value = String::new();
    let mut pending = String::new();
    let mut pending_overflow = false;
    let mut invalid = false;
    scanner.string(|bytes| {
        for character in std::str::from_utf8(bytes)
            .map_err(|_| "invalid_json")?
            .chars()
        {
            if invalid {
                continue;
            }
            if character.is_whitespace() {
                if !value.is_empty() {
                    if pending.len() + character.len_utf8() <= 256 {
                        pending.push(character);
                    } else {
                        pending_overflow = true;
                    }
                }
            } else if pending_overflow
                || character.is_control()
                || pending.chars().any(char::is_control)
                || value.len() + pending.len() + character.len_utf8() > 256
            {
                invalid = true;
            } else {
                value.push_str(&pending);
                pending.clear();
                value.push(character);
            }
        }
        Ok(())
    })?;
    Ok((!invalid && !value.is_empty()).then_some(value))
}

/// Project only billing fields; unknown metadata remains in the output spans.
/// Invalid types remain invalid rather than being turned into absent counters.
fn usage_projection(scanner: &mut Scanner, node: &Node) -> Result<serde_json::Value> {
    use serde_json::{Map, Value};
    Ok(match &node.kind {
        Kind::Null => Value::Null,
        Kind::Bool(value) => Value::Bool(*value),
        Kind::Number => {
            // Only an integral i64 can be a valid reported counter. Longer
            // spellings cannot become a valid integer under the existing parser.
            let length = node.span.end - node.span.start;
            if length > 20 {
                Value::Null
            } else {
                scanner.seek(node.span.start)?;
                let mut bytes = vec![0; length as usize];
                scanner
                    .reader
                    .read_exact(&mut bytes)
                    .map_err(|_| "compact_file_io")?;
                serde_json::from_slice(&bytes).unwrap_or(Value::Null)
            }
        }
        Kind::String(_) => Value::String(
            small_string(scanner, node, 256)?
                .unwrap_or_else(|| "invalid_oversized_field".to_owned()),
        ),
        Kind::Array(_) => Value::Array(Vec::new()),
        Kind::Object(fields) => {
            let mut values = Map::new();
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "usage"
                        | "message"
                        | "response"
                        | "service_tier"
                        | "input_tokens"
                        | "prompt_tokens"
                        | "output_tokens"
                        | "completion_tokens"
                        | "prompt_cache_hit_tokens"
                        | "prompt_cache_miss_tokens"
                        | "input_tokens_details"
                        | "prompt_tokens_details"
                        | "cache_read_input_tokens"
                        | "cache_creation_input_tokens"
                        | "cache_creation"
                        | "cached_tokens"
                        | "ephemeral_5m_input_tokens"
                        | "ephemeral_1h_input_tokens"
                        | "cache_read_observed"
                        | "cache_usage_complete"
                ) {
                    values.insert(key.clone(), usage_projection(scanner, value)?);
                }
            }
            Value::Object(values)
        }
    })
}

/// The caller owns temporary-file lifecycle and async I/O. Returns a rewound output file.
#[cfg(test)]
fn translate(input: File, output: File, wrap_as_sse: bool) -> Result<TranslatedCompact> {
    translate_cancellable(
        input,
        output,
        tempfile::tempfile().unwrap(),
        wrap_as_sse,
        Arc::new(AtomicBool::new(false)),
    )
}

pub(super) fn translate_cancellable(
    input: File,
    mut output: File,
    index_file: File,
    wrap_as_sse: bool,
    cancelled: Arc<AtomicBool>,
) -> Result<TranslatedCompact> {
    let mut scanner = Scanner::with_index(input, index_file)?;
    scanner.index.as_mut().ok_or("compact_file_io")?.cancelled = cancelled.clone();
    scanner.cancelled = cancelled;
    scanner.seek(0)?;
    let root = scanner.value(Capture::Root, 0)?;
    scanner.whitespace()?;
    if scanner.peek()?.is_some() {
        return Err("invalid_json");
    }
    if root
        .get("error")
        .is_some_and(|n| !matches!(n.kind, Kind::Null))
    {
        return Err("provider_error");
    }
    scanner.completed(&root)?;
    let nodes = match root.get("output").map(|n| &n.kind) {
        Some(Kind::Array(nodes)) if nodes.count != 0 => *nodes,
        _ => return Err("compaction_item_missing"),
    };
    let mut native = false;
    let mut at = nodes.head;
    while let Some(node) = scanner.disk_node(&mut at)? {
        let kind = node
            .get("type")
            .filter(|n| n.nonempty_string())
            .ok_or("output_item_malformed")?;
        scanner.completed(&node)?;
        if scanner.equals(kind, "compaction")? {
            native = true;
            if !node
                .get("encrypted_content")
                .is_some_and(Node::nonempty_string)
            {
                return Err("compaction_item_malformed");
            }
        }
    }
    let projected = usage_projection(&mut scanner, &root)?;
    let (usage, usage_invalid) =
        match crate::api::proxy::response_metadata::usage_from_value_checked(&projected) {
            Ok(usage) => (usage, false),
            Err(()) => (None, true),
        };
    let response_id = match root.get("id") {
        Some(node) if node.string() => response_identity(&mut scanner, node)?,
        _ => Some("resp_compact".to_owned()),
    };
    if wrap_as_sse && response_id.is_none() {
        return Err("upstream_invalid_response");
    }
    output.set_len(0).map_err(|_| "compact_file_io")?;
    output
        .seek(SeekFrom::Start(0))
        .map_err(|_| "compact_file_io")?;
    let layout = {
        let mut out = BufWriter::with_capacity(CHUNK, &mut output);
        let layout = emit_response(&mut scanner, &mut out, &root, nodes, native)?;
        out.flush().map_err(|_| "compact_file_io")?;
        layout
    };
    output
        .seek(SeekFrom::Start(0))
        .map_err(|_| "compact_file_io")?;
    let physical_body_bytes = layout.end;
    let mut index = scanner.index.take().ok_or("compact_file_io")?;
    let cancelled = scanner.cancelled.clone();
    // Parsing and translation are complete: reuse the consumed input file for
    // the small delivery plan and release the entire validation index.
    let plan = scanner.reader.into_inner();
    let (pieces, body_bytes, first_billable_byte_end) =
        response_pieces(layout, wrap_as_sse, &mut index, plan, &cancelled)?;
    Ok(TranslatedCompact {
        file: output,
        pieces,
        physical_body_bytes,
        evidence: ValidatedCompact {
            usage,
            usage_invalid,
            response_id,
            body_bytes,
            first_billable_byte_end,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::new_api_transport::{compact_to_responses, responses_to_sse};
    use serde_json::{Value, json};

    #[test]
    fn metadata_keys_and_many_output_items_use_disk_cursors() {
        let long_key = "k".repeat(CHUNK * 2);
        let mut value = json!({"output":[], "usage":{"input_tokens":5}});
        value
            .as_object_mut()
            .unwrap()
            .insert(long_key.clone(), json!({"nested":"preserved"}));
        for number in 0..1024 {
            value
                .as_object_mut()
                .unwrap()
                .insert(format!("unknown_{number}"), json!(number));
            value["output"].as_array_mut().unwrap().push(
                json!({"type":"compaction","encrypted_content":format!("checkpoint_{number}")}),
            );
        }
        let mut input = tempfile::tempfile().unwrap();
        input
            .write_all(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        input.rewind().unwrap();
        let mut scanner = Scanner::new(input);
        let root = scanner.value(Capture::Root, 0).unwrap();
        let Kind::Object(fields) = &root.kind else {
            panic!("object");
        };
        assert_eq!(fields.len(), 2); // Only output and usage are retained in memory.
        let Kind::Array(items) = &root.get("output").unwrap().kind else {
            panic!("array");
        };
        assert_eq!(items.count, 1024);
        assert_eq!(root.raw_fields.unwrap().count, 1026);
        let mut input = scanner.reader.into_inner();
        input.rewind().unwrap();
        let translated = translate(input, tempfile::tempfile().unwrap(), false).unwrap();
        let mut body = materialize(translated);
        body.rewind().unwrap();
        let actual: Value = serde_json::from_reader(body).unwrap();
        assert_eq!(actual[&long_key], value[&long_key]);
        assert_eq!(actual["output"], value["output"]);
        assert_eq!(actual["unknown_1023"], 1023);
    }

    #[test]
    fn escaped_duplicate_keys_and_large_duplicate_keys_remain_invalid() {
        let long_key = "x".repeat(CHUNK * 2);
        for body in [
            r#"{"metadata":{"a":1,"\u0061":2},"output":[{"type":"compaction","encrypted_content":"x"}]}"#.to_owned(),
            format!("{{\"{long_key}\":1,\"{long_key}\":2,\"output\":[{{\"type\":\"compaction\",\"encrypted_content\":\"x\"}}]}}"),
        ] {
            let mut input = tempfile::tempfile().unwrap();
            input.write_all(body.as_bytes()).unwrap();
            assert!(matches!(translate(input, tempfile::tempfile().unwrap(), true), Err("duplicate_json_key")));
        }
    }

    #[test]
    fn delivery_boundary_excludes_created_and_incomplete_item_prefixes() {
        let body = br#"{"output":[{"type":"compaction","encrypted_content":"opaque"}]}"#;
        for sse in [false, true] {
            let mut input = tempfile::tempfile().unwrap();
            input.write_all(body).unwrap();
            let translated = translate(input, tempfile::tempfile().unwrap(), sse).unwrap();
            let boundary = translated.evidence.first_billable_byte_end() as usize;
            let mut bytes = Vec::new();
            materialize(translated).read_to_end(&mut bytes).unwrap();
            if sse {
                let prefix = std::str::from_utf8(&bytes[..boundary]).unwrap();
                assert!(prefix.ends_with("}\n\n"));
                assert!(prefix.contains("event: response.output_item.added\n"));
                assert!(!prefix.contains("event: response.output_item.done\n"));
                assert!(
                    std::str::from_utf8(&bytes[boundary..])
                        .unwrap()
                        .starts_with("event: response.output_item.done\n")
                );
            } else {
                assert_eq!(boundary, bytes.len());
            }
        }
    }

    #[test]
    fn finite_number_semantics_match_unique_json() {
        for number in [
            "1e999",
            "-1e999",
            "1.2e3",
            "0e999",
            "18446744073709551616",
            "1e-999",
        ] {
            let body = format!(
                "{{\"unknown\":{number},\"output\":[{{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}]}}"
            );
            let expected = crate::api::sse::parse_unique_json(body.as_bytes()).is_ok();
            let mut input = tempfile::tempfile().unwrap();
            input.write_all(body.as_bytes()).unwrap();
            assert_eq!(
                translate(input, tempfile::tempfile().unwrap(), false).is_ok(),
                expected,
                "{number}"
            );
        }
    }

    #[test]
    fn cancellation_interrupts_large_string_at_next_chunk() {
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(b"\"").unwrap();
        input.write_all(&vec![b'x'; CHUNK * 3]).unwrap();
        input.write_all(b"\"").unwrap();
        input.rewind().unwrap();
        let mut scanner = Scanner::new(input);
        let cancelled = scanner.cancelled.clone();
        let mut consumed = 0usize;
        let result = scanner.string(|bytes| {
            consumed += bytes.len();
            cancelled.store(true, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(result, Err("compact_cancelled"));
        assert!(consumed <= CHUNK);
        assert!(scanner.position < (CHUNK * 3) as u64);
    }

    fn materialize(mut translated: TranslatedCompact) -> File {
        let mut output = tempfile::tempfile().unwrap();
        while let Some(piece) = read_piece(&mut translated.pieces).unwrap() {
            match piece {
                CompactPiece::Bytes(bytes) => output.write_all(&bytes).unwrap(),
                CompactPiece::Range(range) => {
                    translated.file.seek(SeekFrom::Start(range.start)).unwrap();
                    let copied = std::io::copy(
                        &mut (&mut translated.file).take(range.end - range.start),
                        &mut output,
                    )
                    .unwrap();
                    assert_eq!(copied, range.end - range.start);
                }
            }
        }
        assert_eq!(
            output.metadata().unwrap().len(),
            translated.evidence.expected_body_bytes()
        );
        output.rewind().unwrap();
        output
    }

    fn run(bytes: &[u8], sse: bool) -> Result<Vec<u8>> {
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(bytes).unwrap();
        let mut output = materialize(translate(input, tempfile::tempfile().unwrap(), sse)?);
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).unwrap();
        Ok(bytes)
    }

    fn events(bytes: &[u8]) -> Vec<Value> {
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn file_translation_matches_existing_json_and_every_sse_event() {
        let fixtures = [
            json!({"id":"native","future":{"nested":[1,2]},"usage":{"input_tokens":8},"output":[
                {"type":"compaction","encrypted_content":"first","extra":true},
                {"type":"message","content":"untouched"},
                {"type":"compaction","encrypted_content":"second"},
                {"type":"future","payload":{"a":"b"}}
            ]}),
            json!({"id":"legacy\"","output":[
                {"type":"message","content":[{"type":"output_text","text":"a"},{"type":"text","text":"中😀\n"},{"text":"bc"}]},
                {"type":"reasoning","summary":[]},
                {"type":"message","content":"second"},
                {"type":"message","content":[{"type":"refusal","text":"keep"}]},
                {"type":"message","content":""}
            ]}),
            json!({"id":123,"status":null,"error":null,"output":[
                {"type":"message","content":[{"type":123,"text":"accepted by existing semantics"}]}
            ]}),
        ];
        for compact in fixtures {
            // Pretty JSON exercises whitespace removal for the single-line SSE payload.
            let bytes = serde_json::to_vec_pretty(&compact).unwrap();
            let expected = compact_to_responses(&compact).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&run(&bytes, false).unwrap()).unwrap(),
                expected
            );
            assert_eq!(
                events(&run(&bytes, true).unwrap()),
                events(&responses_to_sse(&expected).unwrap())
            );
        }
        let escaped = br#"{"output":[{"type":"message","content":"a\uD83D\uDE00\u4E2D\nb"}]}"#;
        let expected = compact_to_responses(&serde_json::from_slice(escaped).unwrap()).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&run(escaped, false).unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn rejects_duplicate_keys_and_invalid_json_even_in_unknown_metadata() {
        for bytes in [
            &br#"{"output":[],"output":[]}"#[..],
            &br#"{"output":[],"unknown":{"x":1,"\u0078":2}}"#[..],
            &br#"{"output":[],"unknown":"\uD800"}"#[..],
            &br#"{"output":[],"unknown":01}"#[..],
            &br#"{"output":[],"unknown":[true,]}"#[..],
            &br#"{"output":[]} trailing"#[..],
        ] {
            assert!(run(bytes, false).is_err());
        }
    }

    #[test]
    fn generated_sse_preserves_the_existing_response_identity_contract() {
        for id in ["", "   ", "inside\ncontrol", &"x".repeat(257)] {
            let compact =
                json!({"id":id,"output":[{"type":"compaction","encrypted_content":"opaque"}]});
            let bytes = serde_json::to_vec(&compact).unwrap();
            assert_eq!(run(&bytes, true).unwrap_err(), "upstream_invalid_response");
            assert!(run(&bytes, false).is_ok());
        }
        let id = format!("{}response-id{}", " ".repeat(1024), " ".repeat(1024));
        let bytes = serde_json::to_vec(
            &json!({"id":id,"output":[{"type":"compaction","encrypted_content":"opaque"}]}),
        )
        .unwrap();
        assert!(run(&bytes, true).is_ok());
    }

    #[test]
    fn rejects_unfinished_and_malformed_results() {
        for (compact, expected) in [
            (
                json!({"error":{"code":"bad"},"output":[]}),
                "provider_error",
            ),
            (
                json!({"status":"in_progress","output":[]}),
                "provider_not_completed",
            ),
            (
                json!({"incomplete_details":{},"output":[]}),
                "provider_not_completed",
            ),
            (json!({"output":[]}), "compaction_item_missing"),
            (json!({"output":[null]}), "output_item_malformed"),
            (
                json!({"output":[{"type":"compaction","encrypted_content":""}]}),
                "compaction_item_malformed",
            ),
            (
                json!({"output":[{"type":"message","status":"failed","content":"partial"}]}),
                "provider_not_completed",
            ),
        ] {
            assert_eq!(
                run(&serde_json::to_vec(&compact).unwrap(), false).unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn legacy_text_crosses_reader_and_base64_boundaries_without_losing_parts() {
        use base64::Engine as _;
        let text = "a".repeat(CHUNK - 7);
        let compact = json!({"id":"boundary","output":[
            {"type":"message","content":[
                {"type":"output_text","text":text},
                {"type":"input_text","text":"😀"},
                {"text":"b\n"},
                {"type":null,"text":"结束"}
            ]},
            {"type":"message","content":"next"}
        ]});
        let response: Value =
            serde_json::from_slice(&run(&serde_json::to_vec(&compact).unwrap(), false).unwrap())
                .unwrap();
        let encoded = response["output"][0]["encrypted_content"]
            .as_str()
            .unwrap()
            .strip_prefix("mtc-compact-v1.")
            .unwrap();
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            format!("{text}😀b\n结束")
        );
        assert_eq!(response, compact_to_responses(&compact).unwrap());
    }

    #[test]
    fn verified_usage_preserves_missing_invalid_and_cache_accounting() {
        let fixtures = [
            json!({"output":[{"type":"compaction","encrypted_content":"opaque"}]}),
            json!({"usage":null,"output":[{"type":"compaction","encrypted_content":"opaque"}]}),
            json!({"usage":{"input_tokens":"8","output_tokens":2},"output":[{"type":"compaction","encrypted_content":"opaque"}]}),
            json!({"usage":{"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":40}},"service_tier":"default","output":[{"type":"compaction","encrypted_content":"opaque"}]}),
            json!({"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_cache_hit_tokens":40,"prompt_cache_miss_tokens":60},"output":[{"type":"compaction","encrypted_content":"opaque"}]}),
            json!({"usage":{"input_tokens":100,"output_tokens":7,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":3},"cache_read_input_tokens":4},"output":[{"type":"compaction","encrypted_content":"opaque"}]}),
            json!({"usage":{"input_tokens":100,"output_tokens":7,"input_tokens_details":null},"output":[{"type":"compaction","encrypted_content":"opaque"}]}),
            json!({"response":{"usage":{"input_tokens":100,"output_tokens":7},"service_tier":"priority"},"output":[{"type":"compaction","encrypted_content":"opaque"}]}),
        ];
        for compact in fixtures {
            let mut input = tempfile::tempfile().unwrap();
            input
                .write_all(&serde_json::to_vec(&compact).unwrap())
                .unwrap();
            let translated = translate(input, tempfile::tempfile().unwrap(), true).unwrap();
            let expected = crate::api::proxy::response_metadata::usage_from_value_checked(&compact);
            let summary = translated.evidence.summary();
            match expected {
                Ok(usage) => {
                    assert!(!summary.usage_invalid);
                    assert_eq!(summary.usage, usage);
                }
                Err(()) => {
                    assert!(summary.usage_invalid);
                    assert!(summary.usage.is_none());
                }
            }
            let expected_bytes = translated.evidence.expected_body_bytes();
            assert_eq!(
                materialize(translated).metadata().unwrap().len(),
                expected_bytes
            );
        }
    }

    #[test]
    fn native_body_over_64_mib_retains_all_checkpoint_events_without_materializing_strings() {
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(br#"{"usage":{"input_tokens":9},"output":[{"id":"first","type":"compaction","encrypted_content":""#).unwrap();
        let block = [b'x'; CHUNK];
        for _ in 0..1025 {
            input.write_all(&block).unwrap();
        }
        input
            .write_all(br#""},{"id":"second","type":"compaction","encrypted_content":"second"}]}"#)
            .unwrap();
        assert!(input.metadata().unwrap().len() > 64 * 1024 * 1024);
        let translated = translate(input, tempfile::tempfile().unwrap(), true).unwrap();
        assert!(translated.file.metadata().unwrap().len() < 66 * 1024 * 1024);
        let output = materialize(translated);
        // Inspect each event via the same bounded scanner; never collect a 64 MiB line.
        let mut scanner = Scanner::new(output);
        let mut event_names = Vec::new();
        let mut second_count = 0;
        let mut large_count = 0;
        loop {
            if scanner.peek().unwrap().is_none() {
                break;
            }
            scanner.literal(b"event: ").unwrap();
            let mut name = Vec::new();
            loop {
                let byte = scanner.byte().unwrap();
                if byte == b'\n' {
                    break;
                }
                name.push(byte);
            }
            event_names.push(String::from_utf8(name).unwrap());
            scanner.literal(b"data: ").unwrap();
            // Capture event.item as an Item by scanning the enclosing object's fields
            // manually. response bodies use Root capture.
            scanner.expect(b'{').unwrap();
            loop {
                let mut key = Vec::new();
                scanner
                    .string(|bytes| {
                        key.extend_from_slice(bytes);
                        Ok(())
                    })
                    .unwrap();
                scanner.expect(b':').unwrap();
                let capture = if key == b"response" {
                    Capture::Root
                } else if key == b"item" {
                    Capture::Item
                } else {
                    Capture::Skip
                };
                let node = scanner.value(capture, 0).unwrap();
                let end = scanner.position;
                let mut nodes = Vec::new();
                if key == b"item" {
                    nodes.push(node);
                } else if let Some(Node {
                    kind: Kind::Array(items),
                    ..
                }) = node.get("output")
                {
                    let mut at = items.head;
                    while let Some(item) = scanner.disk_node(&mut at).unwrap() {
                        nodes.push(item);
                    }
                }
                for item in nodes {
                    let encrypted = item.get("encrypted_content").unwrap();
                    if encrypted.span.end - encrypted.span.start == 1025 * CHUNK as u64 + 2 {
                        large_count += 1;
                    } else {
                        assert!(scanner.equals(encrypted, "second").unwrap());
                        second_count += 1;
                    }
                }
                scanner.seek(end).unwrap();
                match scanner.byte().unwrap() {
                    b'}' => break,
                    b',' => {}
                    byte => panic!("unexpected event delimiter {byte}"),
                }
            }
            scanner.literal(b"\n\n").unwrap();
        }
        assert_eq!(
            event_names,
            [
                "response.created",
                "response.output_item.added",
                "response.output_item.done",
                "response.output_item.added",
                "response.output_item.done",
                "response.completed"
            ]
        );
        assert_eq!(large_count, 3);
        assert_eq!(second_count, 3);
    }
}
