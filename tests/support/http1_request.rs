use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct BoundedHttp1Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: Vec<u8>,
}

pub(crate) async fn read_bounded_http1_request(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<BoundedHttp1Request, String> {
    tokio::time::timeout(READ_TIMEOUT, read_bounded_http1_request_inner(stream))
        .await
        .map_err(|_| "timed out while reading HTTP/1 request".to_owned())?
}

async fn read_bounded_http1_request_inner(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<BoundedHttp1Request, String> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
        if bytes.len() == MAX_REQUEST_BYTES {
            return Err("HTTP/1 request headers exceed test limit".to_owned());
        }
        let mut chunk = [0_u8; 4096];
        let remaining = MAX_REQUEST_BYTES - bytes.len();
        let read_limit = remaining.min(chunk.len());
        let read = stream
            .read(&mut chunk[..read_limit])
            .await
            .map_err(|error| format!("failed to read HTTP/1 request headers: {error}"))?;
        if read == 0 {
            return Err("connection closed before complete HTTP/1 headers".to_owned());
        }
        bytes.extend_from_slice(&chunk[..read]);
    };

    let head = std::str::from_utf8(&bytes[..header_end - 4])
        .map_err(|_| "HTTP/1 request headers are not UTF-8".to_owned())?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "HTTP/1 request line is missing".to_owned())?;
    let mut request_line_parts = request_line.split_whitespace();
    let method = request_line_parts
        .next()
        .ok_or_else(|| "HTTP/1 request method is missing".to_owned())?
        .to_owned();
    let path = request_line_parts
        .next()
        .ok_or_else(|| "HTTP/1 request path is missing".to_owned())?
        .to_owned();
    let version = request_line_parts
        .next()
        .ok_or_else(|| "HTTP/1 request version is missing".to_owned())?;
    if request_line_parts.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err("invalid HTTP/1 request line".to_owned());
    }

    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "invalid HTTP/1 request header".to_owned())?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(format!(
                "chunked or transfer-encoded test requests are unsupported: {value}"
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err("duplicate Content-Length is unsupported".to_owned());
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| "invalid Content-Length".to_owned())?,
            );
        }
    }
    let content_length = content_length.ok_or_else(|| "Content-Length is required".to_owned())?;
    let request_end = header_end
        .checked_add(content_length)
        .ok_or_else(|| "HTTP/1 request length overflow".to_owned())?;
    if request_end > MAX_REQUEST_BYTES {
        return Err("HTTP/1 request exceeds test limit".to_owned());
    }
    if bytes.len() > request_end {
        return Err("pipelined bytes after HTTP/1 request are unsupported".to_owned());
    }
    while bytes.len() < request_end {
        let mut chunk = [0_u8; 4096];
        let remaining = request_end - bytes.len();
        let read_limit = remaining.min(chunk.len());
        let read = stream
            .read(&mut chunk[..read_limit])
            .await
            .map_err(|error| format!("failed to read HTTP/1 request body: {error}"))?;
        if read == 0 {
            return Err("connection closed before complete HTTP/1 body".to_owned());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }

    Ok(BoundedHttp1Request {
        method,
        path,
        body: bytes[header_end..request_end].to_vec(),
    })
}
