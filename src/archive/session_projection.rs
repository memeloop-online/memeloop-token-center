//! Worker-only semantic projection. Raw captures are temporary extraction sources;
//! decoded long strings spill in fixed chunks instead of being accumulated in
//! Bytes, RawValue and Value at the same time. Markers never leave this module.
use super::{ArchiveStore, ArchiveWriter, CasArchiveObject, ConversationBodyReference};
use crate::{
    conversation::{PrefixNode, SemanticAtom, SessionMediaLocator},
    error::AppError,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
};
use uuid::Uuid;

const CHUNK: usize = 64 * 1024;

fn invalid() -> AppError {
    AppError::storage("session_body_json_invalid")
}
fn io_error(_: impl std::fmt::Display) -> AppError {
    AppError::storage("session_body_stream_failed")
}

struct ObjectReader {
    runtime: tokio::runtime::Handle,
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = io::Result<Bytes>> + Send>>,
    chunk: Bytes,
}
impl Read for ObjectReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        while self.chunk.is_empty() {
            match self.runtime.block_on(self.stream.next()) {
                Some(chunk) => self.chunk = chunk?,
                None => return Ok(0),
            }
        }
        let count = output.len().min(self.chunk.len());
        output[..count].copy_from_slice(&self.chunk[..count]);
        self.chunk = self.chunk.slice(count..);
        Ok(count)
    }
}

struct Sink<'a> {
    context: &'a Context,
    writer: Option<ArchiveWriter>,
    pending: Vec<u8>,
    chars: u64,
    digest: blake3::Hasher,
}
impl<'a> Sink<'a> {
    fn new(context: &'a Context) -> Self {
        Self {
            context,
            writer: None,
            pending: Vec::with_capacity(CHUNK),
            chars: 0,
            digest: blake3::Hasher::new(),
        }
    }
    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.writer.is_none() {
            let staging = format!(
                "conversation-staging/v1/{}/{}",
                self.context.tenant,
                Uuid::now_v7()
            );
            self.writer = Some(
                self.context
                    .runtime
                    .block_on(self.context.archive.start_compressed_writer(&staging))
                    .map_err(io::Error::other)?,
            );
        }
        self.chars = self
            .chars
            .checked_add(
                self.pending
                    .iter()
                    .filter(|byte| **byte & 0xc0 != 0x80)
                    .count() as u64,
            )
            .ok_or_else(|| io::Error::other("character count overflow"))?;
        let data = Bytes::from(std::mem::replace(
            &mut self.pending,
            Vec::with_capacity(CHUNK),
        ));
        self.context
            .runtime
            .block_on(self.writer.as_mut().unwrap().write(data))
            .map_err(io::Error::other)
    }
    fn finish_temporary(mut self) -> Result<super::StagedArchiveObject, AppError> {
        self.flush_chunk().map_err(io_error)?;
        let writer = self.writer.take().ok_or_else(invalid)?;
        let staged = self.context.runtime.block_on(writer.finish_staged())?;
        self.context
            .temporary
            .lock()
            .unwrap()
            .push(staged.object_locator.clone());
        Ok(staged)
    }

    fn finish(mut self) -> Result<(CasArchiveObject, u64, String), AppError> {
        self.flush_chunk().map_err(io_error)?;
        // JSON may legitimately contain an empty string.
        if self.writer.is_none() {
            let object = self.context.runtime.block_on(
                self.context
                    .archive
                    .put_tenant_content(self.context.tenant, Bytes::new()),
            )?;
            return Ok((object, 0, self.digest.finalize().to_hex().to_string()));
        }
        let staged = self
            .context
            .runtime
            .block_on(self.writer.take().unwrap().finish_staged())?;
        let object = self.context.runtime.block_on(
            self.context
                .archive
                .promote_staged_text_to_cas(self.context.tenant, &staged),
        )?;
        self.context
            .runtime
            .block_on(self.context.archive.delete(&staged.object_locator))?;
        Ok((
            object,
            self.chars,
            self.digest.finalize().to_hex().to_string(),
        ))
    }
}
impl Write for Sink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.digest.update(bytes);
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let count = remaining.len().min(CHUNK - self.pending.len());
            self.pending.extend_from_slice(&remaining[..count]);
            remaining = &remaining[count..];
            if self.pending.len() == CHUNK {
                self.flush_chunk()?;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flush_chunk()
    }
}

struct Context {
    archive: ArchiveStore,
    runtime: tokio::runtime::Handle,
    tenant: Uuid,
    temporary: std::sync::Mutex<Vec<String>>,
}
impl Drop for Context {
    fn drop(&mut self) {
        let objects = std::mem::take(self.temporary.get_mut().unwrap());
        if objects.is_empty() {
            return;
        }
        let archive = self.archive.clone();
        self.runtime.spawn(async move {
            // Only this projection's exact UUID staging objects are temporary.
            // Tenant CAS and raw/conversation roots are never eviction targets.
            for locator in objects {
                if archive.delete(&locator).await.is_err() {
                    tracing::warn!("temporary session projection cleanup deferred");
                }
            }
        });
    }
}
impl Context {
    fn reader(&self, locator: &str) -> Result<Box<dyn Read + Send>, AppError> {
        if let Some(inline) = locator.strip_prefix("inline-json:") {
            return Ok(Box::new(io::Cursor::new(inline.as_bytes().to_vec())));
        }
        let download = self
            .runtime
            .block_on(self.archive.open_stream(locator, None))?;
        Ok(Box::new(ObjectReader {
            runtime: self.runtime.clone(),
            stream: download.stream,
            chunk: Bytes::new(),
        }))
    }
    fn content(&self, object: &CasArchiveObject) -> Result<ObjectReader, AppError> {
        let download = self
            .runtime
            .block_on(self.archive.open_tenant_content(self.tenant, object))?;
        Ok(ObjectReader {
            runtime: self.runtime.clone(),
            stream: download.stream,
            chunk: Bytes::new(),
        })
    }
}

#[derive(Clone)]
enum TextReference {
    Object(super::StagedArchiveObject),
    Concat(Vec<String>),
}
type References = HashMap<String, TextReference>;

struct SequenceReader<'a> {
    context: &'a Context,
    objects: std::collections::VecDeque<super::StagedArchiveObject>,
    current: Option<ObjectReader>,
}
impl Read for SequenceReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(reader) = &mut self.current {
                let count = reader.read(output)?;
                if count != 0 {
                    return Ok(count);
                }
                self.current = None;
            }
            let Some(object) = self.objects.pop_front() else {
                return Ok(0);
            };
            self.current = Some(
                self.context
                    .temporary_content(&object)
                    .map_err(io::Error::other)?,
            );
        }
    }
}
impl Context {
    fn temporary_content(
        &self,
        object: &super::StagedArchiveObject,
    ) -> Result<ObjectReader, AppError> {
        let download = self
            .runtime
            .block_on(self.archive.open_stream(&object.object_locator, None))?;
        if download.object_size != object.size_bytes {
            return Err(invalid());
        }
        let expected = object.blake3_digest.clone();
        let stream = futures_util::stream::try_unfold(
            (download.stream, blake3::Hasher::new()),
            move |(mut source, mut digest)| {
                let expected = expected.clone();
                async move {
                    match source.next().await {
                        Some(chunk) => {
                            let chunk = chunk?;
                            digest.update(&chunk);
                            Ok(Some((chunk, (source, digest))))
                        }
                        None if digest.finalize().to_hex().as_str() != expected => {
                            Err(io::Error::other("temporary projection digest mismatch"))
                        }
                        None => Ok(None),
                    }
                }
            },
        );
        Ok(ObjectReader {
            runtime: self.runtime.clone(),
            stream: Box::pin(stream),
            chunk: Bytes::new(),
        })
    }
}

impl Context {
    fn referenced_reader(
        &self,
        references: &References,
        key: &str,
    ) -> Result<SequenceReader<'_>, AppError> {
        let mut keys = vec![key];
        let mut objects = std::collections::VecDeque::new();
        while let Some(key) = keys.pop() {
            match references.get(key).ok_or_else(invalid)? {
                TextReference::Object(object) => objects.push_back(object.clone()),
                TextReference::Concat(parts) => keys.extend(parts.iter().rev().map(String::as_str)),
            }
        }
        Ok(SequenceReader {
            context: self,
            objects,
            current: None,
        })
    }
}

struct Json<'a, R> {
    source: R,
    context: &'a Context,
    references: &'a mut References,
}
impl<R: BufRead> Json<'_, R> {
    fn peek(&mut self) -> Result<Option<u8>, AppError> {
        Ok(self.source.fill_buf().map_err(io_error)?.first().copied())
    }
    fn byte(&mut self) -> Result<u8, AppError> {
        let byte = self.peek()?.ok_or_else(invalid)?;
        self.source.consume(1);
        Ok(byte)
    }
    fn expect(&mut self, byte: u8) -> Result<(), AppError> {
        if self.byte()? == byte {
            Ok(())
        } else {
            Err(invalid())
        }
    }
    fn whitespace(&mut self) -> Result<(), AppError> {
        while self
            .peek()?
            .is_some_and(|b| matches!(b, b' ' | b'\r' | b'\n' | b'\t'))
        {
            self.source.consume(1);
        }
        Ok(())
    }
    fn string(&mut self, spill: bool) -> Result<String, AppError> {
        self.expect(b'"')?;
        let mut pending = Vec::with_capacity(CHUNK);
        let mut sink = None;
        loop {
            let first = self.byte()?;
            let mut unit = [0_u8; 4];
            let decoded: Vec<u8>;
            let bytes: &[u8] = match first {
                b'"' => break,
                b'\\' => {
                    let escaped = self.byte()?;
                    let mut encoded = vec![b'"', b'\\', escaped];
                    if escaped == b'u' {
                        for _ in 0..4 {
                            encoded.push(self.byte()?);
                        }
                        let scalar = u16::from_str_radix(
                            std::str::from_utf8(&encoded[3..]).map_err(|_| invalid())?,
                            16,
                        )
                        .map_err(|_| invalid())?;
                        if (0xd800..=0xdbff).contains(&scalar) {
                            self.expect(b'\\')?;
                            self.expect(b'u')?;
                            encoded.extend_from_slice(b"\\u");
                            for _ in 0..4 {
                                encoded.push(self.byte()?);
                            }
                        }
                    }
                    encoded.push(b'"');
                    decoded = serde_json::from_slice::<String>(&encoded)
                        .map_err(|_| invalid())?
                        .into_bytes();
                    &decoded
                }
                0..=0x1f => return Err(invalid()),
                first if first < 0x80 => {
                    unit[0] = first;
                    &unit[..1]
                }
                first => {
                    let length = match first {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(invalid()),
                    };
                    unit[0] = first;
                    for byte in &mut unit[1..length] {
                        *byte = self.byte()?;
                    }
                    std::str::from_utf8(&unit[..length]).map_err(|_| invalid())?;
                    &unit[..length]
                }
            };
            pending.extend_from_slice(bytes);
            if spill && pending.len() >= CHUNK {
                let target = sink.get_or_insert_with(|| Sink::new(self.context));
                target.write_all(&pending).map_err(io_error)?;
                pending.clear();
            }
        }
        if let Some(mut sink) = sink {
            sink.write_all(&pending).map_err(io_error)?;
            let object = sink.finish_temporary()?;
            let marker = format!("@mtc-stream:{}@", Uuid::now_v7());
            self.references
                .insert(marker.clone(), TextReference::Object(object));
            Ok(marker)
        } else {
            String::from_utf8(pending).map_err(|_| invalid())
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, AppError> {
        // Match serde_json's structural recursion protection, independent of
        // body length, string length, array size, event count or session size.
        if depth > 128 {
            return Err(invalid());
        }
        self.whitespace()?;
        match self.peek()?.ok_or_else(invalid)? {
            b'"' => Ok(Value::String(self.string(true)?)),
            b'{' => {
                self.source.consume(1);
                self.whitespace()?;
                let mut map = Map::new();
                if self.peek()? == Some(b'}') {
                    self.source.consume(1);
                    return Ok(Value::Object(map));
                }
                loop {
                    let name = self.string(false)?;
                    self.whitespace()?;
                    self.expect(b':')?;
                    let value = self.value(depth + 1)?;
                    map.insert(name, value);
                    self.whitespace()?;
                    match self.byte()? {
                        b'}' => break,
                        b',' => self.whitespace()?,
                        _ => return Err(invalid()),
                    }
                }
                Ok(Value::Object(map))
            }
            b'[' => {
                self.source.consume(1);
                self.whitespace()?;
                let mut values = Vec::new();
                if self.peek()? == Some(b']') {
                    self.source.consume(1);
                    return Ok(Value::Array(values));
                }
                loop {
                    values.push(self.value(depth + 1)?);
                    self.whitespace()?;
                    match self.byte()? {
                        b']' => break,
                        b',' => self.whitespace()?,
                        _ => return Err(invalid()),
                    }
                }
                Ok(Value::Array(values))
            }
            _ => {
                let mut scalar = Vec::new();
                while self.peek()?.is_some_and(|b| {
                    !matches!(b, b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t')
                }) {
                    scalar.push(self.byte()?);
                }
                let value: Value = serde_json::from_slice(&scalar).map_err(|_| invalid())?;
                if matches!(value, Value::Null | Value::Bool(_) | Value::Number(_)) {
                    Ok(value)
                } else {
                    Err(invalid())
                }
            }
        }
    }
    fn document(&mut self) -> Result<Value, AppError> {
        let value = self.value(0)?;
        self.whitespace()?;
        if self.peek()?.is_some() {
            return Err(invalid());
        }
        Ok(value)
    }
}

struct Escaped<'a>(&'a mut dyn Write);
impl Write for Escaped<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut start = 0;
        for (index, byte) in data.iter().copied().enumerate() {
            let escaped: &[u8] = match byte {
                b'"' => b"\\\"",
                b'\\' => b"\\\\",
                b'\n' => b"\\n",
                b'\r' => b"\\r",
                b'\t' => b"\\t",
                8 => b"\\b",
                12 => b"\\f",
                0..=31 => b"",
                _ => continue,
            };
            self.0.write_all(&data[start..index])?;
            if escaped.is_empty() {
                write!(self.0, "\\u{byte:04x}")?;
            } else {
                self.0.write_all(escaped)?;
            }
            start = index + 1;
        }
        self.0.write_all(&data[start..])?;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

fn find_reference<'a>(text: &'a str, references: &References) -> Option<(usize, &'a str)> {
    let mut start = 0;
    while let Some(position) = text[start..].find("@mtc-stream:") {
        let position = start + position;
        let end = text[position + 1..]
            .find('@')
            .map(|end| position + 1 + end + 1)?;
        let key = &text[position..end];
        if references.contains_key(key) {
            return Some((position, key));
        }
        start = position + 1;
    }
    None
}

fn expand_text(
    context: &Context,
    references: &References,
    text: &str,
    output: &mut dyn Write,
) -> Result<(), AppError> {
    let mut remaining = text;
    while !remaining.is_empty() {
        let found = find_reference(remaining, references);
        let Some((position, key)) = found else {
            output.write_all(remaining.as_bytes()).map_err(io_error)?;
            break;
        };
        output
            .write_all(&remaining.as_bytes()[..position])
            .map_err(io_error)?;
        let mut source = context.referenced_reader(references, key)?;
        io::copy(&mut source, output).map_err(io_error)?;
        remaining = &remaining[position + key.len()..];
    }
    Ok(())
}

fn encode(
    context: &Context,
    references: &References,
    value: &Value,
    output: &mut dyn Write,
) -> Result<(), AppError> {
    match value {
        Value::String(text) => {
            output.write_all(b"\"").map_err(io_error)?;
            let mut escaped = Escaped(output);
            if !references.contains_key(text)
                && find_reference(text, references).is_some()
                && let Ok(nested) = serde_json::from_str::<Value>(text)
            {
                // The existing tool normalizer encodes JSON inside a text
                // part. Expand its values before escaping that outer string.
                encode(context, references, &nested, &mut escaped)?;
            } else {
                expand_text(context, references, text, &mut escaped)?;
            }
            output.write_all(b"\"").map_err(io_error)?;
        }
        Value::Array(values) => {
            output.write_all(b"[").map_err(io_error)?;
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.write_all(b",").map_err(io_error)?;
                }
                encode(context, references, value, output)?;
            }
            output.write_all(b"]").map_err(io_error)?;
        }
        Value::Object(values) => {
            output.write_all(b"{").map_err(io_error)?;
            for (index, (key, value)) in values.iter().enumerate() {
                if index != 0 {
                    output.write_all(b",").map_err(io_error)?;
                }
                serde_json::to_writer(&mut *output, key).map_err(io_error)?;
                output.write_all(b":").map_err(io_error)?;
                encode(context, references, value, output)?;
            }
            output.write_all(b"}").map_err(io_error)?;
        }
        value => serde_json::to_writer(output, value).map_err(io_error)?,
    }
    Ok(())
}

pub(crate) struct PreparedSessionBody {
    pub atoms: Vec<SemanticAtom>,
    pub nodes: Vec<PrefixNode>,
    pub media: Vec<SessionMediaLocator>,
    pub request_size: usize,
    pub request_digest: [u8; 32],
    pub publication: super::SemanticPublication,
    pub coverage_complete: bool,
}

impl Context {
    fn source_object(
        &self,
        request: Uuid,
        side: &str,
        locator: &str,
    ) -> Result<CasArchiveObject, AppError> {
        if let Some(object) = self.runtime.block_on(self.archive.raw_capture_reference(
            self.tenant,
            request,
            side,
        ))? && object.object_locator == locator
        {
            return Ok(object);
        }
        let mut reader = self.reader(locator)?;
        let mut digest = blake3::Hasher::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; CHUNK];
        loop {
            let count = reader.read(&mut buffer).map_err(io_error)?;
            if count == 0 {
                break;
            }
            size = size.checked_add(count as u64).ok_or_else(invalid)?;
            digest.update(&buffer[..count]);
        }
        let digest = digest.finalize().to_hex().to_string();
        let object = if locator.strip_prefix("inline-json:").is_some() {
            self.runtime.block_on(self.archive.put_tenant_content(
                self.tenant,
                Bytes::copy_from_slice(locator.strip_prefix("inline-json:").unwrap().as_bytes()),
            ))?
        } else if super::path::is_tenant_cas_location(self.tenant, locator) {
            CasArchiveObject {
                object_locator: locator.into(),
                blake3_digest: digest,
                size_bytes: size,
            }
        } else {
            self.runtime
                .block_on(self.archive.promote_staged_text_to_cas(
                    self.tenant,
                    &super::StagedArchiveObject {
                        object_locator: locator.into(),
                        blake3_digest: digest,
                        size_bytes: size,
                    },
                ))?
        };
        self.runtime.block_on(self.archive.publish_raw_capture(
            self.tenant,
            request,
            side,
            &object,
        ))?;
        Ok(object)
    }

    fn compact(
        &self,
        references: &mut References,
        text: &mut String,
        force: bool,
    ) -> Result<(), AppError> {
        if references.contains_key(text)
            || (text.len() < CHUNK && (!force || find_reference(text, references).is_none()))
        {
            return Ok(());
        }
        let mut remaining = text.as_str();
        let mut pieces = Vec::new();
        while !remaining.is_empty() {
            let found = find_reference(remaining, references)
                .map(|(position, key)| (position, key.to_owned()));
            let (position, key) = found.unwrap_or((remaining.len(), String::new()));
            if position != 0 {
                let mut sink = Sink::new(self);
                sink.write_all(&remaining.as_bytes()[..position])
                    .map_err(io_error)?;
                let object = sink.finish_temporary()?;
                let part = format!("@mtc-stream:{}@", Uuid::now_v7());
                references.insert(part.clone(), TextReference::Object(object));
                pieces.push(part);
            }
            if key.is_empty() {
                break;
            }
            remaining = &remaining[position + key.len()..];
            pieces.push(key);
        }
        let marker = format!("@mtc-stream:{}@", Uuid::now_v7());
        references.insert(marker.clone(), TextReference::Concat(pieces));
        *text = marker;
        Ok(())
    }

    fn media(
        &self,
        references: &References,
        document: &Value,
    ) -> Result<(Vec<super::ConversationMedia>, Vec<String>), AppError> {
        fn decoded(
            context: &Context,
            references: &References,
            text: &str,
            data_url: bool,
        ) -> Result<Option<(String, CasArchiveObject)>, AppError> {
            if data_url && !references.contains_key(text) && !text.starts_with("data:") {
                return Ok(None);
            }
            let input: Box<dyn Read + '_> = if references.contains_key(text) {
                Box::new(context.referenced_reader(references, text)?)
            } else {
                Box::new(io::Cursor::new(text.as_bytes()))
            };
            let mut input = BufReader::with_capacity(CHUNK, input);
            let mut media_type = String::new();
            if data_url {
                let mut prefix = [0_u8; 5];
                if input.read_exact(&mut prefix).is_err() || &prefix != b"data:" {
                    return Ok(None);
                }
                let mut metadata = Vec::new();
                input.read_until(b',', &mut metadata).map_err(io_error)?;
                let Some(kind) = metadata.strip_suffix(b";base64,") else {
                    return Err(AppError::storage("media_data_url_unresolved"));
                };
                media_type = std::str::from_utf8(kind).map_err(|_| invalid())?.into();
                if media_type.is_empty() {
                    return Err(invalid());
                }
            }
            let mut decoder =
                base64::read::DecoderReader::new(input, &base64::engine::general_purpose::STANDARD);
            let mut sink = Sink::new(context);
            io::copy(&mut decoder, &mut sink)
                .map_err(|_| AppError::storage("media_base64_invalid"))?;
            let (object, _, _) = sink.finish()?;
            Ok(Some((media_type, object)))
        }
        fn walk(
            context: &Context,
            references: &References,
            value: &Value,
            pointer: &str,
            media: &mut Vec<super::ConversationMedia>,
            gaps: &mut Vec<String>,
        ) -> Result<(), AppError> {
            match value {
                Value::String(text) => match decoded(context, references, text, true) {
                    Ok(Some((media_type, object))) => media.push(super::ConversationMedia {
                        json_pointer: pointer.into(),
                        media_type,
                        object,
                    }),
                    Ok(None) => {}
                    Err(_) => gaps.push("media_data_url_unresolved".into()),
                },
                Value::Array(values) => {
                    for (index, value) in values.iter().enumerate() {
                        walk(
                            context,
                            references,
                            value,
                            &format!("{pointer}/{index}"),
                            media,
                            gaps,
                        )?;
                    }
                }
                Value::Object(values) => {
                    if let (Some(kind), Some(data)) = (
                        values.get("media_type").and_then(Value::as_str),
                        values.get("data").and_then(Value::as_str),
                    ) {
                        match decoded(context, references, data, false) {
                            Ok(Some((_, object))) => media.push(super::ConversationMedia {
                                json_pointer: format!("{pointer}/data"),
                                media_type: kind.into(),
                                object,
                            }),
                            _ => gaps.push("media_base64_invalid".into()),
                        }
                    }
                    for (name, value) in values {
                        walk(
                            context,
                            references,
                            value,
                            &format!("{pointer}/{}", name.replace('~', "~0").replace('/', "~1")),
                            media,
                            gaps,
                        )?;
                    }
                    let kind = values.get("type").and_then(Value::as_str);
                    if matches!(
                        kind,
                        Some("image_url" | "input_image" | "url" | "file" | "input_file")
                    ) && values.iter().any(|(name, value)| {
                        if !matches!(name.as_str(), "url" | "image_url" | "file_url" | "file_id") {
                            return false;
                        }
                        let location = format!("{pointer}/{name}");
                        let location = if value.is_object() {
                            format!("{location}/url")
                        } else {
                            location
                        };
                        // A spilled string marker only locates decoded text.
                        // Only successfully decoded media at this exact field
                        // proves capture; a long external URL is still a gap.
                        !media.iter().any(|item| item.json_pointer == location)
                    }) {
                        gaps.push("external_media_not_captured".into());
                    }
                }
                _ => {}
            }
            Ok(())
        }
        let mut media = Vec::new();
        let mut gaps = Vec::new();
        walk(self, references, document, "", &mut media, &mut gaps)?;
        // Keep binary data in its original raw capture even when a media URL
        // cannot be resolved; an unresolved dependency never certifies fidelity.
        Ok((media, gaps))
    }
}

struct HashWriter(blake3::Hasher);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn substitute_media(
    document: &mut Value,
    media: &[super::ConversationMedia],
    substitutes: &mut HashMap<Vec<u8>, super::ConversationMedia>,
) {
    use base64::Engine;
    for item in media {
        let Some(value) = document.pointer_mut(&item.json_pointer) else {
            continue;
        };
        let nonce = Uuid::now_v7().as_bytes().to_vec();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&nonce);
        *value = if item.json_pointer.ends_with("/data") {
            Value::String(encoded)
        } else {
            Value::String(format!("data:{};base64,{encoded}", item.media_type))
        };
        substitutes.insert(nonce, item.clone());
    }
}

struct SseFrame<'a, R> {
    source: &'a mut R,
    data: bool,
    line_start: bool,
    frame_end: bool,
    emitted: bool,
    pending: Option<u8>,
}
impl<R: BufRead> Read for SseFrame<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.frame_end {
            return Ok(0);
        }
        let mut count = 0;
        while count < output.len() {
            if let Some(byte) = self.pending.take() {
                output[count] = byte;
                count += 1;
                continue;
            }
            if self.line_start {
                let mut field = [0_u8; 5];
                let mut length = 0;
                loop {
                    let Some(byte) = self.source.fill_buf()?.first().copied() else {
                        self.frame_end = true;
                        return Ok(count);
                    };
                    self.source.consume(1);
                    if byte == b'\n' {
                        if length == 0 || (length == 1 && field[0] == b'\r') {
                            self.frame_end = true;
                            return Ok(count);
                        }
                        break;
                    }
                    if byte == b':' {
                        self.data = length == 4 && &field[..4] == b"data";
                        if self.source.fill_buf()?.first() == Some(&b' ') {
                            self.source.consume(1);
                        }
                        if self.data && self.emitted {
                            self.pending = Some(b'\n');
                        }
                        self.emitted |= self.data;
                        self.line_start = false;
                        break;
                    }
                    if length < field.len() {
                        field[length] = byte;
                    }
                    length = length.saturating_add(1);
                }
                continue;
            }
            let Some(byte) = self.source.fill_buf()?.first().copied() else {
                self.frame_end = true;
                return Ok(count);
            };
            self.source.consume(1);
            if byte == b'\n' {
                self.line_start = true;
                self.data = false;
                continue;
            }
            if byte == b'\r' && self.source.fill_buf()?.first() == Some(&b'\n') {
                continue;
            }
            if self.data {
                output[count] = byte;
                count += 1;
            }
        }
        Ok(count)
    }
}

impl ArchiveStore {
    /// Parse retained source objects without a whole-body read or a total body
    /// ceiling. Each long decoded string is written through a bounded writer.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prepare_session_body_streamed(
        &self,
        tenant: Uuid,
        request: Uuid,
        hints: crate::conversation::ConversationHints,
        request_locator: String,
        response_locator: Option<String>,
        response_complete: bool,
    ) -> Result<Option<PreparedSessionBody>, AppError> {
        let context = Context {
            archive: self.clone(),
            runtime: tokio::runtime::Handle::current(),
            tenant,
            temporary: std::sync::Mutex::new(Vec::new()),
        };
        tokio::task::spawn_blocking(move || {
            context.prepare(
                request,
                hints,
                &request_locator,
                response_locator.as_deref(),
                response_complete,
            )
        })
        .await
        .map_err(|_| AppError::storage("session_body_projection_task_failed"))?
    }
}

impl Context {
    fn prepare(
        &self,
        request: Uuid,
        hints: crate::conversation::ConversationHints,
        request_locator: &str,
        response_locator: Option<&str>,
        response_complete: bool,
    ) -> Result<Option<PreparedSessionBody>, AppError> {
        let raw_request = self.source_object(request, "request", request_locator)?;
        let request_size = usize::try_from(raw_request.size_bytes).map_err(|_| invalid())?;
        let request_digest = *blake3::Hash::from_hex(&raw_request.blake3_digest)
            .map_err(|_| invalid())?
            .as_bytes();
        let mut references = References::new();
        let mut source = Json {
            source: BufReader::with_capacity(CHUNK, self.content(&raw_request)?),
            context: self,
            references: &mut references,
        };
        let mut request_document = match source.document() {
            Ok(value) => value,
            Err(error) if matches!(&error, AppError::Storage { code, .. } if *code == "session_body_json_invalid") =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let (request_media, request_gaps) = self.media(&references, &request_document)?;
        let mut gaps = request_gaps;
        if !response_complete {
            gaps.push("response_not_complete".into());
        }
        let mut media_substitutes = HashMap::new();
        substitute_media(
            &mut request_document,
            &request_media,
            &mut media_substitutes,
        );
        let mut body = crate::conversation::SessionBody::from_request_document(request_document);
        let mut response_valid = true;
        let mut protocol_terminal = false;
        let mut protocol_incomplete = false;
        if let Some(locator) = response_locator {
            let raw_response = self.source_object(request, "response", locator)?;
            let mut source = BufReader::with_capacity(CHUNK, self.content(&raw_response)?);
            while source
                .fill_buf()
                .map_err(io_error)?
                .first()
                .is_some_and(u8::is_ascii_whitespace)
            {
                source.consume(1);
            }
            let mut response_document = if source
                .fill_buf()
                .map_err(io_error)?
                .first()
                .is_some_and(|byte| matches!(byte, b'{' | b'['))
            {
                let parsed = Json {
                    source,
                    context: self,
                    references: &mut references,
                }
                .document();
                match parsed {
                    Ok(document) => {
                        protocol_terminal = document.get("status").and_then(Value::as_str)
                            == Some("completed")
                            || document.get("stop_reason").is_some_and(|v| !v.is_null())
                            || document
                                .get("choices")
                                .and_then(Value::as_array)
                                .is_some_and(|choices| {
                                    !choices.is_empty()
                                        && choices.iter().all(|choice| {
                                            choice
                                                .get("finish_reason")
                                                .is_some_and(|v| !v.is_null())
                                        })
                                });
                        protocol_incomplete = matches!(
                            document.get("status").and_then(Value::as_str),
                            Some("incomplete" | "in_progress" | "failed")
                        );
                        let (media, event_gaps) = self.media(&references, &document)?;
                        gaps.extend(event_gaps);
                        let mut document = document;
                        substitute_media(&mut document, &media, &mut media_substitutes);
                        Some(document)
                    }
                    Err(error) if matches!(&error, AppError::Storage { code, .. } if *code == "session_body_json_invalid") =>
                    {
                        response_valid = false;
                        None
                    }
                    Err(error) => return Err(error),
                }
            } else {
                let mut accumulator = crate::conversation::ReturnDocumentAccumulator::default();
                let mut events = 0_u64;
                while !source.fill_buf().map_err(io_error)?.is_empty() {
                    let frame = SseFrame {
                        source: &mut source,
                        data: false,
                        line_start: true,
                        frame_end: false,
                        emitted: false,
                        pending: None,
                    };
                    let mut frame = BufReader::with_capacity(CHUNK, frame);
                    let prefix = frame.fill_buf().map_err(io_error)?;
                    if prefix.is_empty() {
                        continue;
                    }
                    if prefix.starts_with(b"[DONE]") {
                        protocol_terminal = true;
                        let mut suffix = [0_u8; 6];
                        frame.read_exact(&mut suffix).map_err(io_error)?;
                        while !frame.fill_buf().map_err(io_error)?.is_empty() {
                            if !frame
                                .fill_buf()
                                .map_err(io_error)?
                                .iter()
                                .all(u8::is_ascii_whitespace)
                            {
                                response_valid = false;
                            }
                            let count = frame.fill_buf().map_err(io_error)?.len();
                            frame.consume(count);
                        }
                        continue;
                    }
                    let mut parser = Json {
                        source: &mut frame,
                        context: self,
                        references: &mut references,
                    };
                    let document = parser.document();
                    match document {
                        Ok(mut document) => {
                            let event_type = document.get("type").and_then(Value::as_str);
                            protocol_terminal |= matches!(
                                event_type,
                                Some("response.completed" | "response.done" | "message_stop")
                            );
                            protocol_incomplete |= matches!(
                                event_type,
                                Some("response.incomplete" | "response.failed" | "error")
                            );
                            events = events.checked_add(1).ok_or_else(invalid)?;
                            let (media, event_gaps) = self.media(&references, &document)?;
                            gaps.extend(event_gaps);
                            substitute_media(&mut document, &media, &mut media_substitutes);
                            accumulator.push(document);
                            accumulator.try_map_strings(|text| {
                                self.compact(&mut references, text, false)
                            })?;
                        }
                        Err(error) if matches!(&error, AppError::Storage { code, .. } if *code == "session_body_json_invalid") =>
                        {
                            response_valid = false;
                            io::copy(&mut frame, &mut io::sink()).map_err(io_error)?;
                        }
                        Err(error) => return Err(error),
                    }
                }
                if events == 0 {
                    response_valid = false;
                }
                accumulator.try_map_strings(|text| self.compact(&mut references, text, true))?;
                accumulator.finish_with_tool_input(|input| {
                    if references.contains_key(input) {
                        let reader = self.referenced_reader(&references, input).ok()?;
                        Json {
                            source: BufReader::with_capacity(CHUNK, reader),
                            context: self,
                            references: &mut references,
                        }
                        .document()
                        .ok()
                    } else {
                        serde_json::from_str(input).ok()
                    }
                })
            };
            if let Some(document) = response_document.take() {
                body.append_return_document(document);
            }
            if response_valid {
                gaps.retain(|gap| {
                    gap != "response_missing"
                        && (gap != "response_not_complete" || !response_complete)
                });
            } else {
                gaps.push("response_document_invalid".into());
            }
        } else if !gaps.iter().any(|gap| gap == "response_missing") {
            gaps.push("response_missing".into());
        }
        if !protocol_terminal || protocol_incomplete {
            gaps.push("response_protocol_incomplete".into());
        }
        let pending = body.take_media_bytes();
        for (turn_index, part_index, bytes) in pending {
            let reference = if let Some(reference) = media_substitutes.get(&bytes) {
                reference.object.clone()
            } else {
                self.runtime.block_on(
                    self.archive
                        .put_tenant_content(self.tenant, Bytes::from(bytes)),
                )?
            };
            let part = &mut body.turns[turn_index].parts[part_index];
            part.byte_length = Some(reference.size_bytes);
            part.content_blake3 = Some(reference.blake3_digest.clone());
            part.object_locator = Some(reference.object_locator);
        }
        let (mut atoms, _, mut media) = body.into_archive_parts();
        let mut origins = Vec::with_capacity(atoms.len());
        for atom in &mut atoms {
            let old_hash = atom.content_hash.clone();
            let origin = atom.content.as_object_mut().unwrap().remove("origin");
            let mut digest = HashWriter(blake3::Hasher::new());
            encode(self, &references, &atom.content, &mut digest)?;
            atom.content_hash = digest.0.finalize().to_hex().to_string();
            atom.instance_hash = atom.content_hash.clone();
            origins.push(
                origin
                    .and_then(|origin| origin.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "request".into()),
            );
            if atom
                .content
                .get("parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part.get("completeness").and_then(Value::as_str) == Some("partial")
                    })
                })
            {
                gaps.push("partial_semantic_part".into());
            }
            let mut sink = Sink::new(self);
            encode(self, &references, &atom.content, &mut sink)?;
            let (object, chars, _) = sink.finish()?;
            atom.content = serde_json::to_value(ConversationBodyReference {
                conversation_body_v1: object,
                chars,
            })
            .map_err(|_| invalid())?;
            for item in &mut media {
                if item.turn_id == old_hash {
                    item.turn_id = atom.content_hash.clone();
                }
            }
        }
        let nodes = crate::conversation::build_prefix(&atoms);
        gaps.sort();
        gaps.dedup();
        if atoms.is_empty() {
            gaps.push("semantic_body_empty".into());
        }
        let coverage_complete = response_complete
            && protocol_terminal
            && !protocol_incomplete
            && response_valid
            && gaps.is_empty();
        let backend_identity = atoms
            .first()
            .map(|atom| {
                let reference: ConversationBodyReference =
                    serde_json::from_value(atom.content.clone()).map_err(|_| invalid())?;
                Ok::<_, AppError>(
                    super::routing::split(&reference.conversation_body_v1.object_locator)?
                        .map(|(identity, _)| identity.to_owned()),
                )
            })
            .transpose()?
            .flatten();
        let mut manifest = super::SemanticManifestV2 {
            format_version: 2,
            projection_version: 2,
            tenant_id: self.tenant,
            session_id: hints
                .session_id
                .clone()
                .unwrap_or_else(|| request.to_string()),
            branch_id: hints.branch_id.clone().unwrap_or_else(|| "main".into()),
            parent_branch_id: None,
            parent_turn_id: hints.parent_turn_id.clone(),
            request_id: request,
            revision: 1,
            backend_identity,
            semantic_head: nodes.last().map(|node| super::SemanticHead {
                node_hash: node.node_hash.clone(),
                depth: node.depth as u64,
            }),
            instances_head: None,
            coverage: super::SemanticCoverage {
                observed_terminal: response_complete && protocol_terminal && !protocol_incomplete,
                projection_state: if coverage_complete {
                    super::SemanticProjectionState::Complete
                } else {
                    super::SemanticProjectionState::Partial
                },
                omissions: Vec::new(),
                gaps,
            },
        };
        for node in &nodes {
            self.runtime
                .block_on(self.archive.put_semantic_prefix_node(
                    &manifest,
                    &super::SemanticNode {
                        node_hash: node.node_hash.clone(),
                        parent_hash: node.parent_hash.clone(),
                        atom_hash: node.atom_hash.clone(),
                        depth: node.depth as u64,
                    },
                ))?;
        }
        for start in (0..atoms.len()).step_by(64) {
            let mut instances = Vec::new();
            for ordinal in start..(start + 64).min(atoms.len()) {
                let atom = &atoms[ordinal];
                let node = &nodes[ordinal];
                let reference: ConversationBodyReference =
                    serde_json::from_value(atom.content.clone()).map_err(|_| invalid())?;
                instances.push(super::SemanticTurnInstance {
                    role: Some(atom.role.clone()),
                    instance_id: format!("{request}:{ordinal}:{}", node.node_hash),
                    request_id: request,
                    attempt_id: None,
                    message_id: None,
                    turn_id: hints.turn_id.clone(),
                    branch_id: manifest.branch_id.clone(),
                    parent_instance_id: ordinal.checked_sub(1).map(|previous| {
                        format!("{request}:{previous}:{}", nodes[previous].node_hash)
                    }),
                    origin: origins[ordinal].clone(),
                    ordinal: ordinal as u64,
                    atom_hash: atom.content_hash.clone(),
                    node_hash: node.node_hash.clone(),
                    terminal_state: if response_complete {
                        "ended"
                    } else {
                        "incomplete"
                    }
                    .into(),
                    body: reference.conversation_body_v1,
                });
            }
            manifest.instances_head = Some(self.runtime.block_on(
                self.archive.put_semantic_instance_page(
                    &manifest,
                    &super::SemanticInstancePage {
                        format_version: 2,
                        tenant_id: self.tenant,
                        session_id: manifest.session_id.clone(),
                        branch_id: manifest.branch_id.clone(),
                        first_ordinal: start as u64,
                        previous: manifest.instances_head.clone(),
                        instances,
                    },
                ),
            )?);
        }
        // Coverage and metadata changes produce a new immutable revision; retry
        // of the same publication recovers the exact same small root.
        let revision_hash = blake3::hash(&serde_json::to_vec(&manifest).map_err(|_| invalid())?);
        manifest.revision = u64::from_le_bytes(
            revision_hash.as_bytes()[..8]
                .try_into()
                .map_err(|_| invalid())?,
        )
        .max(1);
        let publication = self
            .runtime
            .block_on(self.archive.publish_semantic_manifest(&manifest))?;
        Ok(Some(PreparedSessionBody {
            atoms,
            nodes,
            media,
            request_size,
            request_digest,
            publication,
            coverage_complete,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ArchiveBackend, Config},
        conversation::{ConversationHints, SessionBody},
    };

    async fn atom_document(store: &ArchiveStore, tenant: Uuid, atom: &SemanticAtom) -> Value {
        let reference: ConversationBodyReference =
            serde_json::from_value(atom.content.clone()).unwrap();
        let bytes = store
            .get_tenant_content(tenant, &reference.conversation_body_v1, 16 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn streamed_json_matches_existing_text_and_nested_tool_normalization() {
        let store = ArchiveStore::memory_for_test();
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let text = "工具😀\n\"\\".repeat(20000);
        let document = serde_json::json!({"messages":[{"role":"user","content":text},{"role":"assistant","tool_calls":[{"id":"call-x","type":"function","function":{"name":"search","arguments":text}}]},{"role":"tool","tool_call_id":"call-x","content":text}]});
        let locator = store
            .put_tenant_content(tenant, Bytes::from(serde_json::to_vec(&document).unwrap()))
            .await
            .unwrap()
            .object_locator;
        let prepared = store
            .prepare_session_body_streamed(
                tenant,
                request,
                ConversationHints::default(),
                locator,
                None,
                false,
            )
            .await
            .unwrap()
            .unwrap();
        let (expected, nodes, _) =
            SessionBody::from_request_document(document).into_archive_parts();
        assert_eq!(prepared.nodes, nodes);
        for (actual, mut expected) in prepared.atoms.iter().zip(expected) {
            expected.content.as_object_mut().unwrap().remove("origin");
            assert_eq!(actual.content_hash, expected.content_hash);
            assert_eq!(
                atom_document(&store, tenant, actual).await,
                expected.content
            );
        }
        let again = store
            .prepare_session_body_streamed(
                tenant,
                request,
                ConversationHints::default(),
                store
                    .raw_capture_reference(tenant, request, "request")
                    .await
                    .unwrap()
                    .unwrap()
                    .object_locator,
                None,
                false,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.nodes, again.nodes);
        use futures_util::TryStreamExt;
        let prefix = super::super::path::archive_path("conversation-staging/v1").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let pending: Vec<_> = store.inner.list(Some(&prefix)).try_collect().await.unwrap();
                if pending.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("temporary decoded text is evicted after durable CAS publication");
        for atom in &again.atoms {
            atom_document(&store, tenant, atom).await;
        }
    }

    #[tokio::test]
    async fn streamed_sse_reassembles_long_deltas_and_decodes_inline_media() {
        use base64::Engine;
        let store = ArchiveStore::memory_for_test();
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let media = vec![42_u8; 300000];
        let document = serde_json::json!({"messages":[{"role":"user","content":[{"type":"text","text":"image"},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&media))}}]}]});
        let locator = store
            .put_tenant_content(tenant, Bytes::from(serde_json::to_vec(&document).unwrap()))
            .await
            .unwrap()
            .object_locator;
        let fragment = "工具😀\n\"\\".repeat(1000);
        let mut response = String::new();
        for _ in 0..40 {
            response.push_str(&format!("event: chunk\r\ndata: {}\r\n\r\n", serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant","content":fragment,"tool_calls":[{"index":0,"id":"","type":"function","function":{"name":"","arguments":fragment}}]}}]})));
        }
        response.push_str("data: [DONE]\r\n\r\n");
        let expected_return =
            crate::conversation::raw_return_document(response.as_bytes()).unwrap();
        let response_locator = store
            .put_tenant_content(tenant, Bytes::from(response))
            .await
            .unwrap()
            .object_locator;
        let prepared = store
            .prepare_session_body_streamed(
                tenant,
                request,
                ConversationHints::default(),
                locator,
                Some(response_locator),
                true,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.media.len(), 1);
        assert_eq!(
            store
                .get_bounded(&prepared.media[0].object_locator, media.len())
                .await
                .unwrap()
                .as_ref(),
            media
        );
        let mut expected = SessionBody::from_request_document(serde_json::json!({"messages":[]}));
        expected.append_return_document(expected_return);
        let (expected, _, _) = expected.into_archive_parts();
        let returned: Vec<_> = prepared.atoms.iter().skip(1).collect();
        assert_eq!(returned.len(), expected.len());
        for (actual, mut expected) in returned.into_iter().zip(expected) {
            expected.content.as_object_mut().unwrap().remove("origin");
            assert_eq!(actual.content_hash, expected.content_hash);
            assert_eq!(
                atom_document(&store, tenant, actual).await,
                expected.content
            );
        }
        let manifest = store
            .get_semantic_manifest_streamed(tenant, &prepared.publication.manifest)
            .await
            .unwrap();
        assert!(manifest.coverage.observed_terminal);
        assert!(manifest.instances_head.is_some());
    }

    #[tokio::test]
    async fn spilled_external_media_retains_gap_and_original_url() {
        use base64::Engine;
        let store = ArchiveStore::memory_for_test();
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let url = format!("https://media.invalid/{}", "x".repeat(CHUNK * 2));
        let bytes = b"retained inline image";
        let document = serde_json::json!({"messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":url}},
            {"type":"input_file","file_id":"file-provider-only"},
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))}}
        ]}]});
        let original = Bytes::from(serde_json::to_vec(&document).unwrap());
        let locator = store
            .put_tenant_content(tenant, original.clone())
            .await
            .unwrap()
            .object_locator;
        let prepared = store
            .prepare_session_body_streamed(
                tenant,
                request,
                ConversationHints::default(),
                locator.clone(),
                None,
                false,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.media.len(), 1);
        assert_eq!(
            store
                .get(&prepared.media[0].object_locator)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        let manifest = store
            .get_semantic_manifest_streamed(tenant, &prepared.publication.manifest)
            .await
            .unwrap();
        assert!(!prepared.coverage_complete);
        assert!(
            manifest
                .coverage
                .gaps
                .iter()
                .any(|gap| gap == "external_media_not_captured")
        );
        assert!(!manifest.coverage.observed_terminal);
        let atom = atom_document(&store, tenant, &prepared.atoms[0]).await;
        assert!(atom.to_string().contains("https://"));
        assert_eq!(store.get(&locator).await.unwrap(), original);
    }

    #[tokio::test]
    async fn successful_transport_without_protocol_terminal_retains_partial_coverage() {
        let store = ArchiveStore::memory_for_test();
        let tenant = Uuid::now_v7();
        for response in [
            r#"{"status":"incomplete","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"partial"}]}]}"#,
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"}}]}\n\n",
        ] {
            let request = Uuid::now_v7();
            let input = store
                .put_tenant_content(
                    tenant,
                    Bytes::from_static(br#"{"messages":[{"role":"user","content":"hello"}]}"#),
                )
                .await
                .unwrap();
            let output = store
                .put_tenant_content(tenant, Bytes::copy_from_slice(response.as_bytes()))
                .await
                .unwrap();
            let prepared = store
                .prepare_session_body_streamed(
                    tenant,
                    request,
                    ConversationHints::default(),
                    input.object_locator,
                    Some(output.object_locator),
                    true,
                )
                .await
                .unwrap()
                .unwrap();
            assert!(!prepared.coverage_complete);
            let manifest = store
                .get_semantic_manifest_streamed(tenant, &prepared.publication.manifest)
                .await
                .unwrap();
            assert!(!manifest.coverage.observed_terminal);
            assert_eq!(
                manifest.coverage.projection_state,
                super::super::SemanticProjectionState::Partial
            );
            assert!(
                manifest
                    .coverage
                    .gaps
                    .iter()
                    .any(|gap| gap == "response_protocol_incomplete")
            );
            assert!(!prepared.atoms.is_empty());
        }
    }

    #[tokio::test]
    async fn streamed_projection_has_no_former_128_mib_body_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::for_test("sqlite::memory:".into());
        config.archive_backend = ArchiveBackend::Filesystem;
        config.archive_path = Some(directory.path().to_string_lossy().into_owned());
        let store = ArchiveStore::from_config(&config).await.unwrap();
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let mut writer = store
            .start_writer("staging/projection-large/body.json")
            .await
            .unwrap();
        writer
            .write(Bytes::from_static(
                br#"{"messages":[{"role":"user","content":""#,
            ))
            .await
            .unwrap();
        let chunk = Bytes::from(vec![b'x'; CHUNK]);
        let mut expected = blake3::Hasher::new();
        expected.update(br#"{"parts":[{"kind":"text","part_index":0,"text":""#);
        for _ in 0..2080 {
            writer.write(chunk.clone()).await.unwrap();
            expected.update(&chunk);
        }
        writer.write(Bytes::from_static(br#""}]}"#)).await.unwrap();
        expected.update(br#""}],"role":"user"}"#);
        let source = writer.finish_staged().await.unwrap();
        let prepared = store
            .prepare_session_body_streamed(
                tenant,
                request,
                ConversationHints::default(),
                source.object_locator,
                None,
                false,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(prepared.request_size > 128 * 1024 * 1024);
        assert_eq!(prepared.atoms.len(), 1);
        assert_eq!(
            prepared.atoms[0].content_hash,
            expected.finalize().to_hex().to_string()
        );
        let reference: ConversationBodyReference =
            serde_json::from_value(prepared.atoms[0].content.clone()).unwrap();
        assert!(reference.conversation_body_v1.size_bytes > 128 * 1024 * 1024);
        store
            .verify_tenant_content(tenant, &reference.conversation_body_v1)
            .await
            .unwrap();
    }
}
