# Request correlation and pre-admission diagnostics

These observations identify waiting stages, not a proven cause of a particular
client failure. No retry, deadline, admission, billing or archival rule changes.

## Control and Gateway are different request chains

The Control request list/query, request detail, upstream directory and request
event stream return a server-owned `x-mtc-request-id`, including early failures.
`control_entry` logs a fixed route class and a separately validated incoming
UUID `ingress_request_id`; `control_response_headers` records status and elapsed
time. SSE response-header completion does not mean its event stream completed.
No concrete URI/query, credentials or body is logged. Unrelated management routes
are excluded. These IDs are diagnostic and do not create model request records.

The Requests page distinguishes list/detail/directory/event-stream errors and
shows HTTP status plus the validated service ID when available. A rejected
ingress request may lack that ID. A plain-text non-JSON failure is represented
as HTTP status, never copied into the error message. An interrupted SSE body
after HTTP 200 is identified as such, not presented as an HTTP 503.

## Model request stages

Existing `request_preparation` and `route_preparation` remain outer totals.
Their nested stages are `application_plugin_snapshot`, `request_json_parse`,
`request_traffic_policy` (including input clone), `authorized_candidate_query`,
`authorized_candidate_preparation`, `group_routing_preparation`, and
`model_price_lookup`. Do not sum nested durations with their parent totals.

`candidate_preparation_exhausted` records static counters for examined, retired,
unavailable snapshot, protocol mismatch and incompatible usage candidates. The
event uses the existing request ID; initial rejection still performs no request
record/diagnostic DB write. Candidate preparation errors retain their existing
safe error-category logs. These counts describe this preparation walk, not all
historical failures of an account.

`candidate_recovery_wait` emits one summary per bounded unsent wait: outcome,
checks, elapsed time, completed check time and completed timer time. Outcomes
distinguish capacity rejection, deadline, readiness, ineligibility, check error
and cancellation (`not_completed`). Check time includes owned-task scheduling
and DB work; it is not pure SQL time. Cancellation can interrupt an active check
or timer, so completed subtotals need not equal total elapsed. Existing ownership
retains a pending DB check after caller cancellation; this summary does not
claim that task has finished. There is no per-poll INFO logging.

Ordinary `/v1/responses` is only identified as compaction by existing explicit
safe hints. `/v1/responses/compact` remains an unsupported, separately classified
path; duration alone does not identify compaction. Correlate a real client UTC
event and request ID before attributing latency or changing configured limits.
The existing downstream Body-poll and delivery-prepare/confirm phases from the
earlier diagnostic change remain unchanged; none establishes TCP acknowledgement.
