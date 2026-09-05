use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use http::{HeaderMap, StatusCode, Version};

pub(super) type UpstreamByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, ()>> + Send + 'static>>;

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
            stream: Box::pin(stream::iter(chunks)),
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

    pub(super) fn bytes_stream(self) -> UpstreamByteStream {
        match self {
            Self::Reqwest(response) => {
                Box::pin(response.bytes_stream().map(|chunk| chunk.map_err(|_| ())))
            }
            Self::Codex(response) => {
                Box::pin(response.bytes_stream().map(|chunk| chunk.map_err(|_| ())))
            }
            Self::Prefetched { stream, .. } => stream,
        }
    }
}

impl From<reqwest::Response> for UpstreamResponse {
    fn from(response: reqwest::Response) -> Self {
        Self::Reqwest(response)
    }
}
