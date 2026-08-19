use std::time::Duration;

pub(super) const REQUEST_ID_HEADER: &str = "x-mtc-request-id";
pub(super) const MAX_SUBSCRIPTION_BRIDGE_RESPONSE: usize = 16 * 1024 * 1024;
pub(super) const MAX_IMAGE_RESPONSE: usize = 16 * 1024 * 1024;
pub(super) const MAX_CPA_IMPORT_BODY: usize = 34 * 1024 * 1024;
pub(super) const MAX_ARCHIVE_DETAIL_RESPONSE: usize = 4 * 1024 * 1024;
pub(super) const MAX_PROXY_RESPONSE_BODY: usize = 64 * 1024 * 1024;
pub(super) const MAX_DEFAULT_REQUEST_BODY: usize = 4 * 1024 * 1024;
pub(super) const MAX_IMAGE_REQUEST_BODY: usize = 16 * 1024 * 1024;
pub(super) const MAX_RESPONSES_SSE_EVENT_BYTES: usize = 256 * 1024;
pub(super) const SYNCHRONOUS_IMAGE_DEADLINE: Duration = Duration::from_secs(12 * 60);
pub(super) const GATEWAY_IN_FLIGHT_REQUESTS: usize = 16;
pub(super) const CONTROL_IN_FLIGHT_REQUESTS: usize = 16;
pub(super) const MAX_REPORTED_TOKENS: i64 = 1_000_000_000;
pub(super) static IMAGE_RESPONSE_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(2);
