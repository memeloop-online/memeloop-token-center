use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use http::{HeaderMap, StatusCode, Version};

pub(super) type UpstreamByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, &'static str>> + Send + 'static>>;

pub(super) const UPSTREAM_STREAM_ERROR: &str = "upstream_stream_read_error";
pub(super) const UPSTREAM_HTTP2_RESET: &str = "upstream_http2_reset";
pub(super) const UPSTREAM_HTTP2_GOAWAY: &str = "upstream_http2_goaway";
pub(super) const UPSTREAM_READ_TIMEOUT: &str = "upstream_read_timeout";
pub(super) const UPSTREAM_REQUEST_TIMEOUT: &str = "upstream_request_timeout";

pub(super) fn terminal_cause_from_stream_error(
    error: &'static str,
) -> Option<crate::model::RequestTerminalCause> {
    use crate::model::RequestTerminalCause;
    match error {
        UPSTREAM_HTTP2_RESET => Some(RequestTerminalCause::Http2Reset),
        UPSTREAM_HTTP2_GOAWAY => Some(RequestTerminalCause::Http2GoAway),
        UPSTREAM_READ_TIMEOUT => Some(RequestTerminalCause::ReadTimeout),
        UPSTREAM_REQUEST_TIMEOUT => Some(RequestTerminalCause::RequestTimeout),
        UPSTREAM_STREAM_ERROR => Some(RequestTerminalCause::StreamReadError),
        _ => None,
    }
}

pub(super) async fn rejected_response_terminal_cause(
    response: UpstreamResponse,
) -> Option<crate::model::RequestTerminalCause> {
    let mut stream = response.bytes_stream();
    let observed = async {
        let mut discarded_bytes = 0usize;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Err(error) => return terminal_cause_from_stream_error(error),
                Ok(bytes) => {
                    discarded_bytes = discarded_bytes.saturating_add(bytes.len());
                    if discarded_bytes >= 64 * 1024 {
                        return None;
                    }
                }
            }
        }
        None
    };
    tokio::time::timeout(std::time::Duration::from_millis(100), observed)
        .await
        .ok()
        .flatten()
}

// The delivery reader may reject an oversized/non-JSON error or stop after a
// terminal event. Keep ownership of its original, deadline-bearing stream so
// that dropping that reader cannot turn the retained original into a prefix.
type CaptureAppend = Pin<
    Box<
        dyn Future<
                Output = (
                    Option<crate::response_archive_spool::ResponseArchiveProducer>,
                    Bytes,
                ),
            > + Send,
    >,
>;

struct CapturedUpstream {
    upstream: Option<UpstreamByteStream>,
    capture: Option<crate::response_archive_spool::ResponseArchiveProducer>,
    pending: Option<CaptureAppend>,
}

impl Stream for CapturedUpstream {
    type Item = Result<Bytes, &'static str>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(pending) = this.pending.as_mut() {
                let (writer, bytes) = match pending.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(value) => value,
                };
                this.pending.take();
                this.capture = writer;
                return Poll::Ready(Some(Ok(bytes)));
            }
            let Some(upstream) = this.upstream.as_mut() else {
                return Poll::Ready(None);
            };
            match upstream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let Some(mut writer) = this.capture.take() else {
                        return Poll::Ready(Some(Ok(bytes)));
                    };
                    // The bounded queue applies backpressure to this one
                    // upstream chunk. A fast or large response must never
                    // turn into a missing archive merely because the upload
                    // queue temporarily filled.
                    this.pending = Some(Box::pin(async move {
                        let retained = writer.append_with_backpressure(bytes.clone()).await;
                        (retained.then_some(writer), bytes)
                    }));
                }
                Poll::Ready(Some(Err(error))) => {
                    this.capture.take();
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => {
                    this.upstream.take();
                    if let Some(writer) = this.capture.take() {
                        drop(writer.seal());
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for CapturedUpstream {
    fn drop(&mut self) {
        let Some(mut upstream) = self.upstream.take() else {
            return;
        };
        let pending = self.pending.take();
        let capture = self.capture.take();
        if pending.is_none() && capture.is_none() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let capture = match pending {
                    Some(pending) => pending.await.0,
                    None => capture,
                };
                let Some(mut writer) = capture else { return };
                while let Some(chunk) = upstream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if !writer.append_with_backpressure(bytes).await {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
                drop(writer.seal());
            });
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CodexHttp2Failure {
    Reset,
    GoAway,
}

impl CodexHttp2Failure {
    pub(super) const fn error_code(self) -> &'static str {
        match self {
            Self::Reset => UPSTREAM_HTTP2_RESET,
            Self::GoAway => UPSTREAM_HTTP2_GOAWAY,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CodexHttp2Evidence {
    reset: bool,
    go_away: bool,
    remote: bool,
    library: bool,
    reason: Option<u32>,
}

impl CodexHttp2Evidence {
    pub(super) const fn is_reset(self) -> bool {
        self.reset
    }

    pub(super) const fn is_go_away(self) -> bool {
        self.go_away
    }

    pub(super) const fn is_remote(self) -> bool {
        self.remote
    }

    pub(super) const fn is_library(self) -> bool {
        self.library
    }

    pub(super) const fn reason(self) -> Option<u32> {
        self.reason
    }
}

pub(super) fn codex_http2_failure(
    error: &(dyn std::error::Error + 'static),
) -> Option<CodexHttp2Failure> {
    let error = codex_http2_error(error)?;
    if error.is_reset() {
        Some(CodexHttp2Failure::Reset)
    } else if error.is_go_away() {
        Some(CodexHttp2Failure::GoAway)
    } else {
        None
    }
}

pub(super) fn codex_http2_error(
    error: &(dyn std::error::Error + 'static),
) -> Option<CodexHttp2Evidence> {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(evidence) = http2_frame_evidence(current) {
            return Some(evidence);
        }
        if let Some(inner) = current
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            source = Some(inner);
            continue;
        }
        source = current.source();
    }
    None
}

fn http2_frame_evidence(error: &(dyn std::error::Error + 'static)) -> Option<CodexHttp2Evidence> {
    if let Some(frame) = error.downcast_ref::<http2::Error>() {
        return frame_evidence(
            frame.is_reset(),
            frame.is_go_away(),
            frame.is_remote(),
            frame.is_library(),
            frame.reason().map(u32::from),
        );
    }
    if let Some(frame) = error.downcast_ref::<h2::Error>() {
        return frame_evidence(
            frame.is_reset(),
            frame.is_go_away(),
            frame.is_remote(),
            frame.is_library(),
            frame.reason().map(u32::from),
        );
    }
    None
}

fn frame_evidence(
    reset: bool,
    go_away: bool,
    remote: bool,
    library: bool,
    reason: Option<u32>,
) -> Option<CodexHttp2Evidence> {
    if reset || go_away {
        Some(CodexHttp2Evidence {
            reset,
            go_away,
            remote,
            library,
            reason,
        })
    } else {
        None
    }
}

pub(super) struct UpstreamResponseParts {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) version: Version,
    pub(super) content_length: Option<u64>,
    pub(super) stream: UpstreamByteStream,
}

/// A small response surface. Native Codex and every other upstream use reqwest.
pub(super) enum UpstreamResponse {
    Reqwest(reqwest::Response),
    Codex(reqwest::Response),
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

    pub(super) fn with_archive_capture(
        self,
        capture: Option<crate::response_archive_spool::ResponseArchiveProducer>,
    ) -> Self {
        let parts = self.into_parts();
        let captured = CapturedUpstream {
            upstream: Some(parts.stream),
            capture,
            pending: None,
        };
        Self::Prefetched {
            status: parts.status,
            headers: parts.headers,
            version: parts.version,
            content_length: parts.content_length,
            stream: Box::pin(captured),
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
        let first_read_deadline = tokio::time::Instant::now()
            .checked_add(read_timeout)
            .unwrap_or(request_deadline);
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
                        if chunk.is_err() {
                            // The caller reports this error before any
                            // settlement/archival await. Replace the state
                            // with an empty stream now so the reqwest body is
                            // not retained by the wrapper after failure.
                            drop(upstream);
                            let empty: UpstreamByteStream = Box::pin(stream::empty());
                            return Some((chunk, (empty, read_deadline, true)));
                        }
                        read_deadline = tokio::time::Instant::now()
                            .checked_add(read_timeout)
                            .unwrap_or(request_deadline);
                        Some((chunk, (upstream, read_deadline, false)))
                    }
                    Ok(None) => None,
                    Err(error_code) => {
                        // Do not keep the underlying response alive while the
                        // caller sends a terminal error or settles storage.
                        drop(upstream);
                        let empty: UpstreamByteStream = Box::pin(stream::empty());
                        Some((Err(error_code), (empty, read_deadline, true)))
                    }
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
            Self::Reqwest(response) => Box::pin(response.bytes_stream().map(|chunk| {
                chunk.map_err(|error| {
                    codex_http2_failure(&error)
                        .map_or(UPSTREAM_STREAM_ERROR, CodexHttp2Failure::error_code)
                })
            })),
            Self::Codex(response) => Box::pin(response.bytes_stream().map(|chunk| {
                chunk.map_err(|error| {
                    codex_http2_failure(&error)
                        .map_or(UPSTREAM_STREAM_ERROR, CodexHttp2Failure::error_code)
                })
            })),
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

    #[tokio::test(start_paused = true)]
    async fn diagnostic_observation_limits_do_not_invent_terminal_causes() {
        let silent = UpstreamResponse::Prefetched {
            status: StatusCode::BAD_GATEWAY,
            headers: HeaderMap::new(),
            version: Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream::pending()),
        };
        assert_eq!(rejected_response_terminal_cause(silent).await, None);
        let bounded = UpstreamResponse::Prefetched {
            status: StatusCode::BAD_GATEWAY,
            headers: HeaderMap::new(),
            version: Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream::iter([
                Ok(Bytes::from(vec![b'x'; 64 * 1024])),
                Err(UPSTREAM_HTTP2_RESET),
            ])),
        };
        assert_eq!(rejected_response_terminal_cause(bounded).await, None);
        assert_eq!(terminal_cause_from_stream_error("http_502"), None);
        assert_eq!(
            terminal_cause_from_stream_error("Bearer untrusted-diagnostic"),
            None
        );
        assert_eq!(terminal_cause_from_stream_error("keepalive_timeout"), None);
    }
    use futures_util::StreamExt;
    use std::{
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll},
    };

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct PendingBody {
        _probe: DropProbe,
    }

    #[tokio::test]
    async fn http2_reset_is_classified_from_a_real_in_memory_frame() {
        let (client_io, server_io) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let mut connection = http2::server::handshake(server_io).await.unwrap();
            let (_, mut response) = connection.accept().await.unwrap().unwrap();
            response.send_reset(http2::Reason::CANCEL);
            let _ = connection.accept().await;
        });
        let (sender, connection) = http2::client::handshake(client_io).await.unwrap();
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut sender = sender.ready().await.unwrap();
        let request = http::Request::post("https://example.test/v1/responses")
            .body(())
            .unwrap();
        let (response, _) = sender.send_request(request, true).unwrap();
        let error = match tokio::time::timeout(std::time::Duration::from_secs(3), response)
            .await
            .expect("RST_STREAM response must terminate")
        {
            Err(error) => error,
            Ok(_) => panic!("RST_STREAM must reject the response"),
        };
        assert!(error.is_reset());
        assert!(error.is_remote());
        assert!(!error.is_library());
        assert_eq!(
            codex_http2_error(&error).unwrap().reason(),
            Some(u32::from(http2::Reason::CANCEL))
        );
        assert_eq!(codex_http2_failure(&error), Some(CodexHttp2Failure::Reset));
        let wrapped = std::io::Error::other(error);
        assert!(codex_http2_error(&wrapped).unwrap().is_remote());
        assert_eq!(
            codex_http2_failure(&wrapped),
            Some(CodexHttp2Failure::Reset)
        );
        driver.abort();
        server.abort();
    }

    #[tokio::test]
    async fn http2_goaway_is_classified_from_a_real_in_memory_frame() {
        let (client_io, server_io) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let mut connection = http2::server::handshake(server_io).await.unwrap();
            connection.abrupt_shutdown(http2::Reason::INTERNAL_ERROR);
            let _ = connection.accept().await;
        });
        let (sender, connection) = http2::client::handshake(client_io).await.unwrap();
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut sender = sender.ready().await.unwrap();
        let request = http::Request::post("https://example.test/v1/responses")
            .body(())
            .unwrap();
        let (response, _) = sender.send_request(request, true).unwrap();
        let error = match tokio::time::timeout(std::time::Duration::from_secs(3), response)
            .await
            .expect("GOAWAY response must terminate")
        {
            Err(error) => error,
            Ok(_) => panic!("GOAWAY must reject the response"),
        };
        assert!(error.is_go_away());
        assert!(error.is_remote());
        assert_eq!(
            codex_http2_error(&error).unwrap().reason(),
            Some(u32::from(http2::Reason::INTERNAL_ERROR))
        );
        assert_eq!(codex_http2_failure(&error), Some(CodexHttp2Failure::GoAway));
        driver.abort();
        server.abort();
    }

    #[tokio::test]
    async fn simulated_http2_stream_error_keeps_one_static_terminal_code() {
        assert_eq!(CodexHttp2Failure::Reset.error_code(), UPSTREAM_HTTP2_RESET);
        assert_eq!(
            CodexHttp2Failure::GoAway.error_code(),
            UPSTREAM_HTTP2_GOAWAY
        );
        for error_code in [UPSTREAM_HTTP2_RESET, UPSTREAM_HTTP2_GOAWAY] {
            let response = UpstreamResponse::Prefetched {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                version: Version::HTTP_2,
                content_length: None,
                stream: Box::pin(stream::iter([
                    Ok(Bytes::from_static(
                        b"data: {\"type\":\"response.created\"}\n\n",
                    )),
                    Err(error_code),
                ])),
            }
            .with_body_timeouts(
                tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(1),
            );
            let mut stream = response.bytes_stream();
            assert!(stream.next().await.unwrap().is_ok());
            assert_eq!(stream.next().await, Some(Err(error_code)));
            assert!(stream.next().await.is_none());
        }
    }

    impl Stream for PendingBody {
        type Item = Result<Bytes, &'static str>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

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
    async fn ten_minute_stream_progress_survives_and_reset_is_terminal_once() {
        let started = tokio::time::Instant::now();
        let parts = delayed_body(vec![std::time::Duration::from_secs(30); 20]).into_parts();
        let response = UpstreamResponse::Prefetched {
            status: parts.status,
            headers: parts.headers,
            version: parts.version,
            content_length: None,
            stream: Box::pin(
                parts
                    .stream
                    .chain(stream::iter([Err(UPSTREAM_HTTP2_RESET)])),
            ),
        }
        .with_body_timeouts(
            started + std::time::Duration::from_secs(900),
            std::time::Duration::from_secs(60),
        );
        let mut body = response.bytes_stream();
        for _ in 0..20 {
            assert_eq!(body.next().await, Some(Ok(Bytes::from_static(b"chunk"))));
        }
        assert_eq!(
            tokio::time::Instant::now() - started,
            std::time::Duration::from_secs(600)
        );
        assert_eq!(body.next().await, Some(Err(UPSTREAM_HTTP2_RESET)));
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn locally_cancelled_http2_stream_is_not_remote_reset_evidence() {
        let (client_io, server_io) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let mut connection = http2::server::handshake(server_io).await.unwrap();
            while connection.accept().await.is_some() {}
        });
        let (sender, connection) = http2::client::handshake(client_io).await.unwrap();
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut sender = sender.ready().await.unwrap();
        let request = http::Request::post("https://example.test/v1/responses")
            .body(())
            .unwrap();
        let (response, mut request_body) = sender.send_request(request, false).unwrap();
        request_body.send_reset(http2::Reason::CANCEL);
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), response)
            .await
            .expect("local cancellation must terminate")
            .unwrap_err();
        let evidence = codex_http2_error(&error).expect("local reset retains typed evidence");
        assert!(evidence.is_reset());
        assert!(!evidence.is_remote());
        assert!(!evidence.is_library());
        assert_eq!(evidence.reason(), Some(u32::from(http2::Reason::CANCEL)));
        assert_eq!(codex_http2_failure(&error), Some(CodexHttp2Failure::Reset));
        driver.abort();
        server.abort();
    }

    #[test]
    fn io_reset_and_bare_reason_do_not_claim_http2_frame_evidence() {
        let error = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(codex_http2_error(&error).is_none());
        assert!(codex_http2_error(&http2::Error::from(http2::Reason::CANCEL)).is_none());
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

    fn largest_representable_read_timeout(now: tokio::time::Instant) -> std::time::Duration {
        let mut minimum = 0_u64;
        let mut maximum = u64::MAX;
        while minimum < maximum {
            let midpoint = minimum + (maximum - minimum) / 2 + 1;
            if now
                .checked_add(std::time::Duration::from_secs(midpoint))
                .is_some()
            {
                minimum = midpoint;
            } else {
                maximum = midpoint - 1;
            }
        }
        std::time::Duration::from_secs(minimum)
    }

    #[tokio::test(start_paused = true)]
    async fn first_read_overflow_after_send_uses_the_frozen_request_deadline() {
        let started = tokio::time::Instant::now();
        let read_timeout = largest_representable_read_timeout(started);
        assert!(started.checked_add(read_timeout).is_some());
        let request_deadline = started + std::time::Duration::from_secs(10);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(
            tokio::time::Instant::now()
                .checked_add(read_timeout)
                .is_none()
        );
        let response = delayed_body(vec![std::time::Duration::from_secs(20)])
            .with_body_timeouts(request_deadline, read_timeout);
        let mut body = response.bytes_stream();
        assert_eq!(body.next().await.unwrap(), Err(UPSTREAM_REQUEST_TIMEOUT));
        assert!(body.next().await.is_none());
        assert_eq!(tokio::time::Instant::now(), request_deadline);
    }

    #[tokio::test(start_paused = true)]
    async fn progress_read_overflow_uses_the_frozen_request_deadline() {
        let started = tokio::time::Instant::now();
        let read_timeout = largest_representable_read_timeout(started);
        assert!(started.checked_add(read_timeout).is_some());
        let request_deadline = started + std::time::Duration::from_secs(10);
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: Version::HTTP_11,
            content_length: None,
            stream: Box::pin(
                stream::iter(vec![
                    Ok(Bytes::from_static(b"first")),
                    Ok(Bytes::from_static(b"second")),
                ])
                .chain(stream::pending()),
            ),
        }
        .with_body_timeouts(request_deadline, read_timeout);
        let mut body = response.bytes_stream();
        assert!(body.next().await.unwrap().is_ok());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(body.next().await.unwrap().is_ok());
        assert!(
            tokio::time::Instant::now()
                .checked_add(read_timeout)
                .is_none()
        );
        assert_eq!(body.next().await.unwrap(), Err(UPSTREAM_REQUEST_TIMEOUT));
        assert!(body.next().await.is_none());
        assert_eq!(tokio::time::Instant::now(), request_deadline);
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
    async fn request_timeout_drops_the_underlying_body_before_returning_error() {
        let dropped = Arc::new(AtomicBool::new(false));
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: Version::HTTP_11,
            content_length: None,
            stream: Box::pin(PendingBody {
                _probe: DropProbe(dropped.clone()),
            }),
        }
        .with_body_timeouts(
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(10),
        );
        let mut body = response.bytes_stream();

        tokio::time::advance(std::time::Duration::from_secs(1)).await;

        assert_eq!(body.next().await.unwrap(), Err(UPSTREAM_REQUEST_TIMEOUT));
        assert!(
            dropped.load(Ordering::SeqCst),
            "timeout must release the underlying upstream body before the caller settles"
        );
        drop(body);
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
