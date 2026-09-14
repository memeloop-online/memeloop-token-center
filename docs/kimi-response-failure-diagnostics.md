# Kimi Responses failure classification

`kimi_response_translation` is emitted once when a successful Kimi HTTP response
cannot be translated. It retains the gateway-owned request ID and ingress clock
even when the response body is polled by another task. No request or response
text, tool arguments, credentials, provider error strings, or usage token counts
are included.

The fixed `stage` distinguishes headers, observe, body_read, eof, and buffered.
The fixed `error_kind` distinguishes SSE limits/framing, JSON/schema failures,
Chat usage/choice sequencing, missing or unsupported finish reason (including
length and content_filter), invalid usage totals, and translation item/memory
limits. `event_class` is a code-owned category, not an upstream event name.
`usage_observed` and `done_observed` indicate presence only, never successful
validation. A DONE rejected for invalid usage therefore still reports observed
DONE. Buffered responses have no DONE marker.

The public `upstream_stream` failure, parsing contract, limits, retry behavior,
and settlement rules are unchanged. The first Chat validation reason is retained
so a later DONE failure does not hide an earlier invalid frame. Neither a missing
completed event nor a contract-ceiling settlement proves that usage was missing
from the upstream wire. These diagnostics do not reconstruct previously lost
raw streams or prove downstream network delivery.
