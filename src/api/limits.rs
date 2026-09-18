use std::time::Duration;

pub(super) const REQUEST_ID_HEADER: &str = "x-mtc-request-id";
pub(super) const MAX_IMAGE_RESPONSE: usize = 16 * 1024 * 1024;
pub(super) const MAX_ARCHIVE_DETAIL_RESPONSE: usize = 4 * 1024 * 1024;
pub(super) const MAX_PROXY_RESPONSE_BODY: usize = 64 * 1024 * 1024;
pub(super) const MAX_DEFAULT_REQUEST_BODY: usize = 4 * 1024 * 1024;
/// A route-level ingress ceiling. The per-process configured responses limit
/// is applied by gateway body admission before the JSON extractor runs.
pub(super) const MAX_RESPONSES_REQUEST_BODY: usize =
    crate::config::MAX_RESPONSES_BODY_MAX_BYTES as usize;
pub(super) const MAX_IMAGE_REQUEST_BODY: usize = 16 * 1024 * 1024;
pub(super) const MAX_AUDIO_RESPONSE_BODY: usize = 4 * 1024 * 1024;
// Responses terminal events may contain the complete response object. Real
// Codex responses already exceed the former 256 KiB ceiling, so keep a
// dedicated, bounded allowance that can carry those valid events without
// weakening the independent 64 MiB response-body admission limit.
pub(super) const MAX_RESPONSES_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;
// Bound decoder products independently of the 64 MiB response budget. This
// prevents tiny legal events or fields from multiplying frame metadata,
// archive batching, and JSON classification work before downstream
// backpressure can apply.
pub(super) const MAX_SSE_FRAMES_PER_NETWORK_CHUNK: usize = 4_096;
// A single legal event must also fit the per-network-chunk decoder product
// ceiling when the HTTP client delivers it without fragmentation. The small
// tail allowance carries DONE and other bounded control framing in that same
// network chunk.
pub(super) const MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK: usize =
    MAX_RESPONSES_SSE_EVENT_BYTES + 64 * 1024;
pub(super) const MAX_SSE_FIELDS_PER_EVENT: usize = 4_096;
// Each emitted event and retained field line owns vector metadata. Keep their
// combined count below a process-safe ceiling even when a bounded network
// chunk is composed of millions of two-byte fields.
pub(super) const MAX_SSE_METADATA_ITEMS_PER_NETWORK_CHUNK: usize = 16 * 1024;
// Successful Responses terminal frames are held until EOF confirms the raw
// framing is complete. Leave bounded room for a maximum-sized terminal plus
// its tiny DONE/control tail while remaining far below the 64 MiB response
// admission ceiling.
pub(super) const MAX_RESPONSES_SSE_TERMINAL_HOLD_BYTES: usize =
    MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK;
pub(super) const SYNCHRONOUS_IMAGE_DEADLINE: Duration = Duration::from_secs(12 * 60);
pub(super) const SYNCHRONOUS_AUDIO_DEADLINE: Duration = Duration::from_secs(10 * 60);
pub(super) const CLOUD_WEBHOOK_BODY_READ_DEADLINE: Duration = Duration::from_secs(10);
pub(super) const MAX_CLOUD_WEBHOOK_BODY: usize = 64 * 1024;
pub(super) const CONTROL_IN_FLIGHT_REQUESTS: usize = 16;
pub(super) const MAX_REPORTED_TOKENS: i64 = 1_000_000_000;
pub(super) static CLOUD_WEBHOOK_BODY_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(4);
