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

The public `upstream_stream` failure, limits, retry behavior,
and settlement rules are unchanged. The first Chat validation reason is retained
so a later DONE failure does not hide an earlier invalid frame. Neither a missing
completed event nor a contract-ceiling settlement proves that usage was missing
from the upstream wire. These diagnostics do not reconstruct previously lost
raw streams or prove downstream network delivery.

Kimi alone accepts complete usage on the terminal choice or the existing
usage-only frame. The default OpenAI usage-only contract is unchanged. A clean
HTTP EOF can replace DONE only after complete SSE framing, exactly one valid
finished choice, consistent full usage, and complete uniquely identified tool
calls with valid JSON arguments. EOF alone, missing usage/finish, a partial SSE
frame, conflicting/duplicate usage, or a transport error cannot yield completed.
`length` and `content_filter` yield response.incomplete, never completed. Existing
conservative settlement of incomplete responses is unchanged and is a separate
accounting follow-up; this change does not claim to fix that financial policy.
