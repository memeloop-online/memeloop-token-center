use super::*;
use axum::extract::multipart::Field;

pub(super) async fn text(
    mut field: Field<'_>,
    name: &str,
    maximum: usize,
) -> Result<String, AppError> {
    let invalid = || AppError::BadRequest(format!("invalid {name} field"));
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|_| invalid())? {
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| invalid())
}

pub(super) async fn hotwords(mut field: Field<'_>) -> Result<Vec<String>, AppError> {
    let invalid =
        || AppError::BadRequest("hotwords must be a JSON string array with valid entries".into());
    // JSON whitespace can be arbitrarily long. Only the current encoded word
    // is retained; six bytes per decoded byte covers every JSON escape.
    let mut state = 0_u8; // array, first word/end, quoted word, comma/end, next word, EOF
    let mut escaped = false;
    let mut word = Vec::new();
    let mut words = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|_| invalid())? {
        for byte in chunk {
            if state == 2 {
                word.push(byte);
                if word.len() > MAX_HOTWORD_BYTES * 6 + 2 {
                    return Err(invalid());
                }
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    let decoded: String = serde_json::from_slice(&word).map_err(|_| invalid())?;
                    if decoded.is_empty()
                        || decoded.len() > MAX_HOTWORD_BYTES
                        || decoded.contains('\0')
                    {
                        return Err(invalid());
                    }
                    words.push(decoded);
                    word.clear();
                    state = 3;
                }
                continue;
            }
            if matches!(byte, b' ' | b'\r' | b'\n' | b'\t') {
                continue;
            }
            state = match (state, byte) {
                (0, b'[') => 1,
                (1 | 3, b']') => 5,
                (1 | 4, b'"') if words.len() < MAX_HOTWORDS => {
                    word.push(byte);
                    2
                }
                (3, b',') => 4,
                _ => return Err(invalid()),
            };
        }
    }
    if state != 5 {
        return Err(invalid());
    }
    Ok(words)
}

pub(super) async fn temperature(
    mut field: Field<'_>,
    directory: &std::path::Path,
) -> Result<f64, AppError> {
    let invalid = || AppError::BadRequest("temperature must be a number from 0 to 1".into());
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|_| AppError::internal("audio_metadata_directory"))?;
    let directory = directory.to_owned();
    let file = tokio::task::spawn_blocking(move || tempfile::tempfile_in(directory))
        .await
        .map_err(|_| AppError::internal("audio_metadata_owner"))?
        .map_err(|_| AppError::internal("audio_metadata_create"))?;
    let mut file = tokio::fs::File::from_std(file);
    let mut small = Some(Vec::new());
    while let Some(chunk) = field.chunk().await.map_err(|_| invalid())? {
        if let Some(bytes) = small.as_mut() {
            if bytes.len().saturating_add(chunk.len()) <= 1024 {
                bytes.extend_from_slice(&chunk);
            } else {
                small = None;
            }
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| AppError::internal("audio_metadata_write"))?;
    }
    if let Some(bytes) = small {
        return parse_temperature(std::str::from_utf8(&bytes).map_err(|_| invalid())?);
    }
    file.flush()
        .await
        .map_err(|_| AppError::internal("audio_metadata_flush"))?;
    file.rewind()
        .await
        .map_err(|_| AppError::internal("audio_metadata_seek"))?;
    let file = file.into_std().await;
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _cancel_on_drop = CancelNumericRead(cancelled.clone());
    let value = tokio::task::spawn_blocking(move || {
        // serde_json's reader parses numeric digits incrementally, without
        // retaining a complete numeric string (arbitrary_precision is disabled).
        serde_json::from_reader::<_, f64>(FloatReader::new(file).with_cancellation(cancelled))
    })
    .await
    .map_err(|_| AppError::internal("audio_metadata_parse_owner"))?
    .map_err(|_| invalid())?;
    validate_temperature(Some(value))?;
    Ok(value)
}

struct CancelNumericRead(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelNumericRead {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

// Rust's f64 syntax also permits a leading plus/zeros and an omitted integer
// or fractional zero. Normalize those spellings without retaining the digits.
struct FloatReader<R: std::io::Read> {
    bytes: std::io::Bytes<std::io::BufReader<R>>,
    pending: std::collections::VecDeque<u8>,
    initialized: bool,
    after_dot: bool,
    seen_digit: bool,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl<R: std::io::Read> FloatReader<R> {
    fn new(reader: R) -> Self {
        Self {
            bytes: std::io::Read::bytes(std::io::BufReader::new(reader)),
            pending: Default::default(),
            initialized: false,
            after_dot: false,
            seen_digit: false,
            cancelled: None,
        }
    }

    fn with_cancellation(
        mut self,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.cancelled = Some(cancelled);
        self
    }

    fn raw_byte(&mut self) -> std::io::Result<Option<u8>> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err(std::io::Error::other("numeric upload owner cancelled"));
        }
        let Some(byte) = self.bytes.next().transpose()? else {
            return Ok(None);
        };
        if byte.is_ascii_whitespace() {
            return Ok(Some(b' '));
        }
        if byte.is_ascii() {
            return Ok(Some(byte));
        }
        let width = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid number text",
                ));
            }
        };
        let mut utf8 = [0_u8; 4];
        utf8[0] = byte;
        for slot in &mut utf8[1..width] {
            *slot = self.bytes.next().transpose()?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid number text")
            })?;
        }
        if std::str::from_utf8(&utf8[..width])
            .ok()
            .and_then(|s| s.chars().next())
            .is_some_and(char::is_whitespace)
        {
            Ok(Some(b' '))
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid number text",
            ))
        }
    }

    fn next_byte(&mut self) -> std::io::Result<Option<u8>> {
        if let Some(byte) = self.pending.pop_front() {
            return Ok(Some(byte));
        }
        if !self.initialized {
            self.initialized = true;
            let mut first = self.raw_byte()?;
            while first.is_some_and(|b| b.is_ascii_whitespace()) {
                first = self.raw_byte()?;
            }
            if matches!(first, Some(b'+' | b'-')) {
                if first == Some(b'-') {
                    self.pending.push_back(b'-');
                }
                first = self.raw_byte()?;
                if matches!(first, Some(b'+' | b'-'))
                    || first.is_some_and(|byte| byte.is_ascii_whitespace())
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "duplicate number sign",
                    ));
                }
            }
            let mut zeros = false;
            while first == Some(b'0') {
                zeros = true;
                first = self.raw_byte()?;
            }
            self.seen_digit = zeros || first.is_some_and(|b| b.is_ascii_digit());
            if first == Some(b'.') || (zeros && !first.is_some_and(|b| b.is_ascii_digit())) {
                self.pending.push_back(b'0');
            }
            if let Some(byte) = first {
                self.pending.push_back(byte);
                self.after_dot = byte == b'.';
            }
            return Ok(self.pending.pop_front());
        }
        let next = self.raw_byte()?;
        if self.after_dot && !next.is_some_and(|b| b.is_ascii_digit()) {
            if !self.seen_digit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing number digits",
                ));
            }
            self.after_dot = false;
            if let Some(byte) = next {
                self.pending.push_back(byte);
            }
            return Ok(Some(b'0'));
        }
        self.seen_digit |= next.is_some_and(|b| b.is_ascii_digit());
        self.after_dot = next == Some(b'.');
        Ok(next)
    }
}

impl<R: std::io::Read> std::io::Read for FloatReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let mut written = 0;
        for slot in output {
            let Some(byte) = self.next_byte()? else {
                break;
            };
            *slot = byte;
            written += 1;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::FromRequest;

    async fn multipart(value: String) -> Multipart {
        let mut request = axum::http::Request::builder()
            .header("content-type", "multipart/form-data; boundary=metadata-test")
            .body(axum::body::Body::from(format!(
                "--metadata-test\r\nContent-Disposition: form-data; name=\"value\"\r\n\r\n{value}\r\n--metadata-test--\r\n"
            ))).unwrap();
        axum::extract::DefaultBodyLimit::disable().apply(&mut request);
        Multipart::from_request(request, &()).await.unwrap()
    }

    #[tokio::test]
    async fn hotwords_ignore_long_whitespace_but_bound_each_decoded_word() {
        let mut input = multipart(format!(
            "[{}\"word\",\"\\u0061\"]",
            " ".repeat(2 * 1024 * 1024)
        ))
        .await;
        assert_eq!(
            hotwords(input.next_field().await.unwrap().unwrap())
                .await
                .unwrap(),
            vec!["word", "a"]
        );
        let mut input = multipart(format!("[\"{}\"]", "x".repeat(2 * 1024 * 1024))).await;
        assert!(
            hotwords(input.next_field().await.unwrap().unwrap())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn prompt_enforces_existing_semantic_size_without_full_collection() {
        let mut input = multipart("x".repeat(2 * 1024 * 1024)).await;
        assert!(
            text(input.next_field().await.unwrap().unwrap(), "prompt", 32_000)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn temperature_accepts_long_equivalent_numeric_spelling() {
        let directory = tempfile::tempdir().unwrap();
        let mut input = multipart(format!("+{}.500{}", "0".repeat(8192), "0".repeat(8192))).await;
        assert_eq!(
            temperature(input.next_field().await.unwrap().unwrap(), directory.path())
                .await
                .unwrap(),
            0.5
        );
    }

    #[test]
    fn cancelled_numeric_owner_stops_before_reading_unbounded_source() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let owner = CancelNumericRead(flag.clone());
        let reader = FloatReader::new(std::io::repeat(b'0')).with_cancellation(flag);
        drop(owner);
        assert!(serde_json::from_reader::<_, f64>(reader).is_err());
    }

    #[test]
    fn streaming_float_normalization_matches_supported_spellings() {
        for input in ["+000.5", ".5", "1.", "0.e4", "000", " -000.0 ", "1e-10000"] {
            let result: f64 = serde_json::from_reader(FloatReader::new(input.as_bytes())).unwrap();
            assert_eq!(result, input.trim().parse::<f64>().unwrap(), "{input}");
        }
        for input in [".", "-.", "1..", "+", ".e2", "+-0", "+ 0", "- 0"] {
            assert!(
                serde_json::from_reader::<_, f64>(FloatReader::new(input.as_bytes())).is_err(),
                "{input}"
            );
        }
    }
}
