use super::*;
use std::io;

type Result<T> = io::Result<T>;
fn invalid() -> io::Error {
    io::Error::other("invalid semantic atom")
}
struct Scanner<R> {
    input: R,
    capture: Option<Window>,
    output: Option<tokio::sync::mpsc::Sender<std::io::Result<ContentPiece>>>,
}
struct Window {
    offset: u64,
    count: u64,
    bytes: Vec<u8>,
    output: Option<tokio::sync::mpsc::Sender<std::io::Result<ContentPiece>>>,
    emitted: u64,
}
impl Window {
    fn push(&mut self, byte: u8) -> Result<()> {
        if self.count >= self.offset && (self.output.is_some() || self.bytes.len() < CHUNK + 4) {
            self.bytes.push(byte);
        }
        self.count += 1;
        if self.output.is_some() && self.bytes.len() >= CHUNK {
            let mut end = CHUNK;
            while std::str::from_utf8(&self.bytes[..end]).is_err() {
                if CHUNK - end >= 3 {
                    return Err(invalid());
                }
                end -= 1;
            }
            let text = std::str::from_utf8(&self.bytes[..end])
                .map_err(|_| invalid())?
                .to_owned();
            self.output
                .as_ref()
                .ok_or_else(invalid)?
                .blocking_send(Ok(ContentPiece {
                    text,
                    offset: self.offset + self.emitted,
                    complete: false,
                }))
                .map_err(|_| invalid())?;
            self.emitted += end as u64;
            self.bytes.drain(..end);
        }
        Ok(())
    }
    fn finish(mut self) -> Result<(String, bool)> {
        if self.offset > self.count {
            return Err(invalid());
        }
        if self.output.is_some() {
            let text = String::from_utf8(self.bytes).map_err(|_| invalid())?;
            return Ok((text, true));
        }
        let mut end = self.bytes.len().min(CHUNK);
        while std::str::from_utf8(&self.bytes[..end]).is_err() {
            if end == 0 || CHUNK.saturating_sub(end) > 3 {
                return Err(invalid());
            }
            end -= 1;
        }
        self.bytes.truncate(end);
        let complete = self.offset + end as u64 == self.count;
        Ok((
            String::from_utf8(self.bytes).map_err(|_| invalid())?,
            complete,
        ))
    }
}
impl<R: BufRead> Scanner<R> {
    fn peek(&mut self) -> Result<Option<u8>> {
        Ok(self.input.fill_buf()?.first().copied())
    }
    fn byte(&mut self) -> Result<u8> {
        let byte = self.peek()?.ok_or_else(invalid)?;
        self.input.consume(1);
        if let Some(window) = &mut self.capture {
            window.push(byte)?;
        }
        Ok(byte)
    }
    fn ws(&mut self) -> Result<()> {
        while self
            .peek()?
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.byte()?;
        }
        Ok(())
    }
    fn expect(&mut self, byte: u8) -> Result<()> {
        self.ws()?;
        if self.byte()? != byte {
            return Err(invalid());
        }
        Ok(())
    }
    // Decodes only one Unicode scalar/escape at a time. Large strings never
    // enter serde's owned String visitor or a whole-value scratch buffer.
    fn string(&mut self, decoded: bool, mut emit: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        self.expect(b'"')?;
        loop {
            let byte = self.byte()?;
            match byte {
                b'"' => return Ok(()),
                0..=31 => return Err(invalid()),
                b'\\' => {
                    let mut escape = vec![b'"', b'\\', self.byte()?];
                    if escape[2] == b'u' {
                        for _ in 0..4 {
                            escape.push(self.byte()?);
                        }
                        let unit = u16::from_str_radix(
                            std::str::from_utf8(&escape[3..]).map_err(|_| invalid())?,
                            16,
                        )
                        .map_err(|_| invalid())?;
                        if (0xD800..=0xDBFF).contains(&unit) {
                            for _ in 0..6 {
                                escape.push(self.byte()?);
                            }
                        }
                    }
                    escape.push(b'"');
                    let value: String = serde_json::from_slice(&escape).map_err(|_| invalid())?;
                    if decoded {
                        emit(value.as_bytes())?;
                    }
                }
                byte if byte < 128 => {
                    if decoded {
                        emit(&[byte])?;
                    }
                }
                byte => {
                    let length = match byte {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(invalid()),
                    };
                    let mut scalar = [0; 4];
                    scalar[0] = byte;
                    for item in scalar.iter_mut().take(length).skip(1) {
                        *item = self.byte()?;
                    }
                    std::str::from_utf8(&scalar[..length]).map_err(|_| invalid())?;
                    if decoded {
                        emit(&scalar[..length])?;
                    }
                }
            }
        }
    }
    fn small_string(&mut self) -> Result<String> {
        let mut bytes = Vec::new();
        self.string(true, |part| {
            if bytes.len() + part.len() > 16384 {
                return Err(invalid());
            }
            bytes.extend_from_slice(part);
            Ok(())
        })?;
        String::from_utf8(bytes).map_err(|_| invalid())
    }
    fn value(&mut self, depth: usize) -> Result<()> {
        if depth > 128 {
            return Err(invalid());
        }
        self.ws()?;
        match self.peek()?.ok_or_else(invalid)? {
            b'"' => self.string(false, |_| Ok(())),
            b'{' => {
                self.byte()?;
                self.ws()?;
                if self.peek()? == Some(b'}') {
                    self.byte()?;
                    return Ok(());
                }
                loop {
                    self.string(false, |_| Ok(()))?;
                    self.expect(b':')?;
                    self.value(depth + 1)?;
                    self.ws()?;
                    match self.byte()? {
                        b'}' => break,
                        b',' => {
                            self.ws()?;
                        }
                        _ => return Err(invalid()),
                    }
                }
                Ok(())
            }
            b'[' => {
                self.byte()?;
                self.ws()?;
                if self.peek()? == Some(b']') {
                    self.byte()?;
                    return Ok(());
                }
                loop {
                    self.value(depth + 1)?;
                    self.ws()?;
                    match self.byte()? {
                        b']' => break,
                        b',' => (),
                        _ => return Err(invalid()),
                    }
                }
                Ok(())
            }
            _ => {
                let mut token = Vec::new();
                while self.peek()?.is_some_and(|b| {
                    !matches!(b, b' ' | b'\n' | b'\r' | b'\t') && !matches!(b, b',' | b']' | b'}')
                }) {
                    if token.len() > 128 {
                        return Err(invalid());
                    }
                    token.push(self.byte()?);
                }
                serde_json::from_slice::<serde_json::Value>(&token).map_err(|_| invalid())?;
                Ok(())
            }
        }
    }
    fn small_value(&mut self) -> Result<serde_json::Value> {
        self.capture = Some(Window {
            offset: 0,
            count: 0,
            bytes: Vec::new(),
            output: None,
            emitted: 0,
        });
        self.value(0)?;
        let window = self.capture.take().ok_or_else(invalid)?;
        if window.count > 16384 {
            return Err(invalid());
        }
        serde_json::from_slice(&window.bytes).map_err(|_| invalid())
    }
    fn part(
        &mut self,
        ordinal: usize,
        selection: &Selection,
        projection: &mut Projection,
    ) -> Result<()> {
        self.expect(b'{')?;
        let selected = match selection {
            Selection::Parts { start } => ordinal >= *start && ordinal - *start < PAGE,
            Selection::Media { part } => ordinal == *part,
            _ => false,
        };
        let mut part = Part {
            part_index: ordinal,
            ordinal,
            ..Default::default()
        };
        let mut names = std::collections::HashSet::new();
        self.ws()?;
        if self.peek()? == Some(b'}') {
            return Err(invalid());
        }
        loop {
            let name = self.small_string()?;
            if names.len() >= 32 || !names.insert(name.clone()) {
                return Err(invalid());
            }
            self.expect(b':')?;
            self.ws()?;
            let text_field = matches!(name.as_str(), "text" | "arguments_text");
            let field = text_field || matches!(name.as_str(), "arguments" | "result_content");
            if field {
                let format = if text_field { "text" } else { "json" };
                if selected {
                    part.fields.push(Field {
                        name: name.clone(),
                        format: format.into(),
                    });
                }
                if let Selection::Content {
                    part: target,
                    field,
                    offset,
                } = selection
                    && *target == ordinal
                    && *field == name
                {
                    if projection.format.is_some() {
                        return Err(invalid());
                    }
                    let mut window = Window {
                        offset: *offset,
                        count: 0,
                        bytes: Vec::with_capacity(CHUNK + 4),
                        output: self.output.take(),
                        emitted: 0,
                    };
                    if text_field {
                        self.string(true, |bytes| {
                            for byte in bytes {
                                window.push(*byte)?;
                            }
                            Ok(())
                        })?;
                    } else {
                        self.capture = Some(window);
                        self.value(0)?;
                        window = self.capture.take().ok_or_else(invalid)?;
                    }
                    projection.stream_end = Some(window.offset + window.emitted);
                    let (text, complete) = window.finish()?;
                    projection.text = text;
                    projection.complete = complete;
                    projection.format = Some(format.into());
                } else {
                    self.value(0)?;
                }
            } else if selected
                && matches!(
                    name.as_str(),
                    "part_index"
                        | "kind"
                        | "tool_name"
                        | "call_id"
                        | "tool_call_id"
                        | "completeness"
                        | "media_type"
                        | "byte_length"
                        | "object_locator"
                        | "content_blake3"
                )
            {
                let value = self.small_value()?;
                match name.as_str() {
                    "part_index" => {
                        part.part_index = usize::try_from(value.as_u64().ok_or_else(invalid)?)
                            .map_err(|_| invalid())?;
                    }
                    "kind" => part.kind = value.as_str().ok_or_else(invalid)?.into(),
                    "tool_name" => part.tool_name = value.as_str().map(str::to_owned),
                    "call_id" | "tool_call_id" => part.call_id = value.as_str().map(str::to_owned),
                    "completeness" => part.completeness = value.as_str().map(str::to_owned),
                    "media_type" => part.media_type = value.as_str().map(str::to_owned),
                    "byte_length" => part.byte_length = value.as_u64(),
                    "object_locator" => part.locator = value.as_str().map(str::to_owned),
                    "content_blake3" => part.digest = value.as_str().map(str::to_owned),
                    _ => (),
                }
            } else {
                self.value(0)?;
            }
            self.ws()?;
            match self.byte()? {
                b'}' => break,
                b',' => {
                    self.ws()?;
                }
                _ => return Err(invalid()),
            }
        }
        if selected {
            if part.kind.is_empty() {
                return Err(invalid());
            }
            projection.parts.push(part);
        }
        if let Selection::Parts { start } = selection
            && ordinal >= start.saturating_add(PAGE)
        {
            projection.more = true;
        }
        Ok(())
    }
}
pub(super) fn scan<R: BufRead>(input: R, selection: Selection) -> Result<Projection> {
    scan_with_output(input, selection, None)
}
pub(super) fn scan_with_output<R: BufRead>(
    input: R,
    selection: Selection,
    output: Option<tokio::sync::mpsc::Sender<std::io::Result<ContentPiece>>>,
) -> Result<Projection> {
    let mut scanner = Scanner {
        input,
        capture: None,
        output,
    };
    let mut projection = Projection {
        role: None,
        parts: Vec::new(),
        more: false,
        text: String::new(),
        complete: false,
        format: None,
        stream_end: None,
    };
    scanner.expect(b'{')?;
    let mut seen = std::collections::HashSet::new();
    loop {
        let key = scanner.small_string()?;
        if seen.len() >= 16 || !seen.insert(key.clone()) {
            return Err(invalid());
        }
        scanner.expect(b':')?;
        match key.as_str() {
            "role" => projection.role = Some(scanner.small_string()?),
            "parts" => {
                scanner.expect(b'[')?;
                scanner.ws()?;
                if scanner.peek()? == Some(b']') {
                    scanner.byte()?;
                } else {
                    let mut ordinal = 0;
                    loop {
                        scanner.part(ordinal, &selection, &mut projection)?;
                        ordinal += 1;
                        scanner.ws()?;
                        match scanner.byte()? {
                            b']' => break,
                            b',' => (),
                            _ => return Err(invalid()),
                        }
                    }
                }
            }
            _ => scanner.value(0)?,
        }
        scanner.ws()?;
        match scanner.byte()? {
            b'}' => break,
            b',' => (),
            _ => return Err(invalid()),
        }
    }
    scanner.ws()?;
    if scanner.peek()?.is_some() || !seen.contains("parts") || projection.role.is_none() {
        return Err(invalid());
    }
    if matches!(selection, Selection::Content { .. }) && projection.format.is_none() {
        return Err(invalid());
    }
    Ok(projection)
}
