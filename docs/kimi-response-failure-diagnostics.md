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

## Documented Chat cache dialect

The [official Kimi Chat API](https://platform.kimi.ai/docs/api/chat) streaming
example (read 2026-09-14, also available as `/docs/api/chat.md`) puts
`cached_tokens` directly inside `usage`, including on the terminal choice.
`src/api/kimi_transport/fixtures/documented-chat-stream.sse` reproduces that
public example, not a customer response. Its 19 prompt tokens include 12 cached
tokens: settlement must preserve 7 uncached + 12 cached + 13 output tokens.
Both DONE and clean-EOF variants pass through actual translation, delivery,
settlement and archive fixtures; cached tokens are not added twice.
The test uses distinct uncached/cached/output prices of 2/1/3 USD per million
and asserts 65 microdollars. Normalized TokenUsage stores 7 uncached inputs;
request records intentionally expose 19 inclusive inputs with a cached subset
of 12. The SSE fixture's final empty line is an intentional protocol delimiter,
not removable trailing prose whitespace.

The Kimi-only parser ignores additional envelope/choice metadata while keeping
delta data and validating the existing core identity/choice/finish contract.
Its documented top-level cached count maps to the canonical nested count. If
both spellings are present they must agree. Unknown accounting fields remain
rejected rather than assumed to be zero or assigned invented pricing semantics.
The default OpenAI strict parser is unchanged. The same cache mapping is used
by the Kimi Responses translator, preventing acceptance with a lost cache count.
The shared Kimi normalizer validates required integer totals, equality and token
limits as well as every supported detail's type/range before returning. Thus
buffered Responses cannot bypass streaming's accounting checks. Unsupported
accounting dimensions (including cache-write counters without a Kimi billing
contract) are rejected, not silently omitted from a completed response.
Native streaming Chat selects the same parser only for the authenticated route's
`kimi-oauth` driver; it still requires its existing DONE terminal contract and
does not synthesize a wire terminal. Kimi buffered Responses also maps the cache
alias through its existing translator. Generic/non-streaming Chat accounting
and Anthropic accounting are not changed by this patch.

Schema failures now surface on the first invalid JSON frame, with static
envelope/choice/usage and missing/type/unknown categories. No Serde error text,
arbitrary provider field name, or field value is logged. The actual request
`01a0a204-711a-7e23-b0e6-628b845dcf51` on 2026-09-14 failed with a retained
`chat_chunk_schema` reason reported at DONE, despite observed usage and DONE.
Its response archive is a gap: that evidence proves schema rejection, not which
field caused it. The documented cache field is independently reproducible, not
claimed as a recovered field from that missing raw stream. Historical ceiling
settlements are not rewritten or refunded by this compatibility change.

The [CPA Kimi executor at 7bbfeaf8](https://github.com/router-for-me/CLIProxyAPI/blob/7bbfeaf8a7acf2cd5a834dcb0842539fe6aabc2b/internal/runtime/executor/kimi_executor.go)
requests stream usage and forwards Chat events to its translator. Its synthetic
DONE behavior is not copied as proof of success after a transport error.
