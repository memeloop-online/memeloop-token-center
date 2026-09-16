use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use http::{HeaderMap, StatusCode, Version};

pub(super) type UpstreamByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, &'static str>> + Send + 'static>>;

pub(super) const UPSTREAM_STREAM_ERROR: &str = "upstream_stream_read_error";
pub(super) const UPSTREAM_READ_TIMEOUT: &str = "upstream_read_timeout";
pub(super) const UPSTREAM_REQUEST_TIMEOUT: &str = "upstream_request_timeout";

pub(super) struct UpstreamResponseParts {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) version: Version,
    pub(super) content_length: Option<u64>,
    pub(super) stream: UpstreamByteStream,
}

/// A small client-neutral response surface. Native Codex uses wreq for its
/// TLS/HTTP2 fingerprint; all other upstreams remain on reqwest.
pub(super) enum UpstreamResponse {
    Reqwest(reqwest::Response),
    Codex(wreq::Response),
    Prefetched {
        status: StatusCode,
        headers: HeaderMap,
        version: Version,
        content_length: Option<u64>,
        stream: UpstreamByteStream,
    },
}

impl UpstreamResponse {
    #[cfg(test)]
    pub(super) fn for_test(
        headers: HeaderMap,
        version: Version,
        chunks: Vec<Result<Bytes, ()>>,
    ) -> Self {
        Self::Prefetched {
            status: StatusCode::OK,
            headers,
            version,
            content_length: None,
            stream: Box::pin(stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| chunk.map_err(|_| UPSTREAM_STREAM_ERROR)),
            )),
        }
    }

    pub(super) fn status(&self) -> StatusCode {
        match self {
            Self::Reqwest(response) => response.status(),
            Self::Codex(response) => response.status(),
            Self::Prefetched { status, .. } => *status,
        }
    }

    pub(super) fn headers(&self) -> &HeaderMap {
        match self {
            Self::Reqwest(response) => response.headers(),
            Self::Codex(response) => response.headers(),
            Self::Prefetched { headers, .. } => headers,
        }
    }

    pub(super) fn version(&self) -> Version {
        match self {
            Self::Reqwest(response) => response.version(),
            Self::Codex(response) => response.version(),
            Self::Prefetched { version, .. } => *version,
        }
    }

    pub(super) fn content_length(&self) -> Option<u64> {
        match self {
            Self::Reqwest(response) => response.content_length(),
            Self::Codex(response) => response.content_length(),
            Self::Prefetched { content_length, .. } => *content_length,
        }
    }

    pub(super) fn into_parts(self) -> UpstreamResponseParts {
        let status = self.status();
        let headers = self.headers().clone();
        let version = self.version();
        let content_length = self.content_length();
        let stream = self.bytes_stream();
        UpstreamResponseParts {
            status,
            headers,
            version,
            content_length,
            stream,
        }
    }

    pub(super) fn from_prefetched_parts(
        mut parts: UpstreamResponseParts,
        prefetched: Vec<Bytes>,
        content_type: http::HeaderValue,
    ) -> Self {
        parts
            .headers
            .insert(http::header::CONTENT_TYPE, content_type);
        let prefixed = stream::iter(prefetched.into_iter().map(Ok)).chain(parts.stream);
        Self::Prefetched {
            status: parts.status,
            headers: parts.headers,
            version: parts.version,
            content_length: parts.content_length,
            stream: Box::pin(prefixed),
        }
    }

    /// Apply one request-wide absolute deadline plus an inactivity window that
    /// starts only after response headers have arrived. The absolute deadline
    /// is created before `send()` and is deliberately carried into the body;
    /// it is never restarted at the header/body boundary.
    pub(super) fn with_body_timeouts(
        self,
        request_deadline: tokio::time::Instant,
        read_timeout: std::time::Duration,
    ) -> Self {
        // Start the first inactivity window when headers arrive, not when a
        // downstream consumer eventually asks for the first body chunk.
        let first_read_deadline = tokio::time::Instant::now() + read_timeout;
        let parts = self.into_parts();
        let timed = stream::unfold(
            (parts.stream, first_read_deadline, false),
            move |(mut upstream, mut read_deadline, finished)| async move {
                if finished {
                    return None;
                }
                let next = tokio::select! {
                    // The total request budget is absolute: once it expires,
                    // buffered or continuously-ready body data cannot extend it.
                    biased;
                    _ = tokio::time::sleep_until(request_deadline) => Err(UPSTREAM_REQUEST_TIMEOUT),
                    // Inactivity is different: data already buffered at the
                    // read boundary is progress and starts a fresh read window.
                    next = upstream.next() => Ok(next),
                    _ = tokio::time::sleep_until(read_deadline) => Err(UPSTREAM_READ_TIMEOUT),
                };
                match next {
                    Ok(Some(chunk)) => {
                        read_deadline = tokio::time::Instant::now() + read_timeout;
                        Some((chunk, (upstream, read_deadline, false)))
                    }
                    Ok(None) => None,
                    Err(error_code) => Some((Err(error_code), (upstream, read_deadline, true))),
                }
            },
        );
        Self::Prefetched {
            status: parts.status,
            headers: parts.headers,
            version: parts.version,
            content_length: parts.content_length,
            stream: Box::pin(timed),
        }
    }

    pub(super) fn bytes_stream(self) -> UpstreamByteStream {
        match self {
            Self::Reqwest(response) => Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(|_| UPSTREAM_STREAM_ERROR)),
            ),
            Self::Codex(response) => Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(|_| UPSTREAM_STREAM_ERROR)),
            ),
            Self::Prefetched { stream, .. } => stream,
        }
    }
}

impl From<reqwest::Response> for UpstreamResponse {
    fn from(response: reqwest::Response) -> Self {
        Self::Reqwest(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn delayed_body(delays: Vec<std::time::Duration>) -> UpstreamResponse {
        let stream = stream::iter(delays).then(|delay| async move {
            tokio::time::sleep(delay).await;
            Ok(Bytes::from_static(b"chunk"))
        });
        UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn read_timeout_starts_after_headers_and_resets_after_body_progress() {
        let now = tokio::time::Instant::now();
        let response = delayed_body(vec![
            std::time::Duration::from_millis(900),
            std::time::Duration::from_millis(900),
            std::time::Duration::from_millis(1_001),
        ])
        .with_body_timeouts(
            now + std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(1),
        );
        let mut body = response.bytes_stream();
        assert!(body.next().await.unwrap().is_ok());
        assert!(body.next().await.unwrap().is_ok());
        assert_eq!(body.next().await.unwrap(), Err(UPSTREAM_READ_TIMEOUT));
        assert!(body.next().await.is_none());
        assert_eq!(
            tokio::time::Instant::now() - now,
            std::time::Duration::from_millis(2_800)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn first_read_timeout_starts_when_headers_arrive() {
        let headers_arrived = tokio::time::Instant::now();
        let response = delayed_body(vec![std::time::Duration::from_secs(2)]).with_body_timeouts(
            headers_arrived + std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(1),
        );
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        let mut body = response.bytes_stream();
        assert_eq!(body.next().await.unwrap(), Err(UPSTREAM_READ_TIMEOUT));
        assert_eq!(
            tokio::time::Instant::now() - headers_arrived,
            std::time::Duration::from_secs(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_deadline_is_not_restarted_when_headers_become_a_body() {
        let send_started = tokio::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let response = delayed_body(vec![std::time::Duration::from_secs(2)]).with_body_timeouts(
            send_started + std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(5),
        );
        let mut body = response.bytes_stream();
        assert_eq!(body.next().await.unwrap(), Err(UPSTREAM_REQUEST_TIMEOUT));
        assert_eq!(
            tokio::time::Instant::now() - send_started,
            std::time::Duration::from_secs(3)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn continuously_ready_body_cannot_cross_the_absolute_request_deadline() {
        let started = tokio::time::Instant::now();
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream::repeat(Ok(Bytes::from_static(b"chunk")))),
        }
        .with_body_timeouts(
            started + std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(10),
        );
        let mut body = response.bytes_stream();
        assert!(body.next().await.unwrap().is_ok());

        tokio::time::advance(std::time::Duration::from_secs(1)).await;

        assert_eq!(body.next().await.unwrap(), Err(UPSTREAM_REQUEST_TIMEOUT));
        assert!(body.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn buffered_progress_wins_at_the_read_inactivity_boundary() {
        let started = tokio::time::Instant::now();
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream::iter([Ok(Bytes::from_static(b"buffered"))])),
        }
        .with_body_timeouts(
            started + std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(1),
        );
        let mut body = response.bytes_stream();

        tokio::time::advance(std::time::Duration::from_secs(1)).await;

        assert_eq!(
            body.next().await.unwrap(),
            Ok(Bytes::from_static(b"buffered"))
        );
        assert!(body.next().await.is_none());
    }
}
