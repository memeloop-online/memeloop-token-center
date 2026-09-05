use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, StatusCode, Version};

pub(super) type UpstreamByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, ()>> + Send + 'static>>;

/// A small client-neutral response surface. Native Codex uses wreq for its
/// TLS/HTTP2 fingerprint; all other upstreams remain on reqwest.
pub(super) enum UpstreamResponse {
    Reqwest(reqwest::Response),
    Codex(wreq::Response),
}

impl UpstreamResponse {
    pub(super) fn status(&self) -> StatusCode {
        match self {
            Self::Reqwest(response) => response.status(),
            Self::Codex(response) => response.status(),
        }
    }

    pub(super) fn headers(&self) -> &HeaderMap {
        match self {
            Self::Reqwest(response) => response.headers(),
            Self::Codex(response) => response.headers(),
        }
    }

    pub(super) fn version(&self) -> Version {
        match self {
            Self::Reqwest(response) => response.version(),
            Self::Codex(response) => response.version(),
        }
    }

    pub(super) fn content_length(&self) -> Option<u64> {
        match self {
            Self::Reqwest(response) => response.content_length(),
            Self::Codex(response) => response.content_length(),
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
        }
    }
}

impl From<reqwest::Response> for UpstreamResponse {
    fn from(response: reqwest::Response) -> Self {
        Self::Reqwest(response)
    }
}
