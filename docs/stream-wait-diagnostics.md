# Streaming application wait diagnostics

`stream_wait_summary` is one metadata event per returned or deadline-cancelled
stream lifecycle, correlated by the existing server-owned `request_id`. It does
not contain body content, headers, URLs, credentials, or error display strings.
Process termination or task panic may prevent the summary. Buffered responses
and pre-header waits are outside this event's scope.

All durations use Tokio's monotonic clock, accumulated before millisecond
truncation. `upstream_pending_ms` measures the lifetime from the first Pending
poll of each body-next future until Ready or cancellation. Immediately-ready
reads contribute zero. `max_upstream_pending_ms` is the longest such interval.
Heartbeat selection can cancel a pending read; its observed interval is retained.

`max_no_body_pending_lower_bound_ms` is the maximum sum of those pending
intervals between observed nonempty upstream body chunks (including the initial
and unfinished final interval). Empty chunks do not reset it. It is a lower bound
on elapsed **application observation time without a nonempty body chunk**, not
on network silence. The sum excludes time outside the body-next future: channel
sends, database work, parsing, archive work, and other consumption pauses. It is
not a single continuously-polled interval; heartbeat sends can separate its
constituent intervals. `nonempty_body_chunks` counts raw application chunks,
excluding sanitizer terminal replay. These are not HTTP/2 frames.

`downstream_capacity_pending_ms` and `max_downstream_capacity_pending_ms` measure
Pending lifetimes of normal/held-terminal frame channel send/reserve and progress
heartbeat sends, including their existing timeout. Delivery database transitions
are excluded. Error-frame sends are not instrumented, so these fields are partial
capacity-wait observations, not total downstream latency. Channel enqueue or
reservation success does not establish receipt by the downstream client.
`progress_heartbeats_enqueued` counts only successful synthetic heartbeat sends.

`transport_terminal` records the stream owner's fixed transport outcome;
`owner_outcome` distinguishes lifecycle return from lifecycle deadline. Neither
proves successful database settlement. Join by request ID to the durable terminal
record; a missing record remains pending, never evidence of a live long stream.

The event always sets `observation_layer=application_body_poll` and
`http2_data_silence_proven=false`. Pending intervals can include executor
scheduling delay, transport-library buffering, and flow control inherited from
earlier slow consumption. Excluding application send time does not exclude all
network backpressure. No duration here proves continuous HTTP/2 DATA silence or
peer liveness. Such proof still requires per-stream transport frame timestamps
and connection/control-frame observations. Policies for deadlines, retries,
heartbeats, delivery and billing are unchanged.

CI tests contrast a pending body across 315 seconds of heartbeat selections with
a ready body and a consumer holding channel capacity for 310 seconds. Virtual
time tests validate accounting, not production transport availability. Existing
real-time HTTP/2 acceptance tests remain separate and unchanged.
