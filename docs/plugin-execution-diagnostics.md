# Component invocation diagnostics

The authenticated traffic-policy and buffered provider prepare/normalize paths
share one host execution wrapper. It preserves the process-wide eight-call
permit pool, one-second capacity wait, and 35-second caller execution deadline.
Wasm fuel, memory, and epoch limits remain independently enforced by the runtime.
No retry, fallback, health mutation, or additional plugin authority is added.

Each admitted wrapper invocation has a host-generated opaque `invocation_id`.
The blocking task enters a `plugin_invocation` tracing span containing that ID
and the closed phase `post_auth`, `prepare`, or `normalize`, allowing existing
sanitized guest log events to be associated with this host invocation. The
caller emits one info-level `plugin_execution_observed` event with those same
fields, elapsed milliseconds, and one closed outcome:

| Outcome | Meaning |
| --- | --- |
| `returned` | Hook returned a value; policy may still deny it, and host response validation remains necessary. This is not successful delivery. |
| `hook_error` | Hook returned an error; original internal control flow is preserved without logging its detail. |
| `capacity_timeout` | No execution permit arrived within the queue deadline; the hook did not execute. |
| `capacity_closed` | Permit pool was closed; the hook did not execute. |
| `execution_timeout` | Caller stopped waiting; blocking work may still own its permit. |
| `task_failed` | Blocking task failed; no panic payload or join-error text is copied into this event or returned task-failure message. |
| `caller_cancelled` | The invocation future was dropped while queued or waiting; this does not prove the blocking task stopped. |

`invocation_id` is neither request ID nor a metrics label. It connects only the
component span and wrapper observation, not an entire failover chain. Events
contain no model, account preference, configuration, bodies, headers, raw plugin
reason, supplier response, or credentials. Filtering the existing tracing
target controls diagnostic visibility; this change adds no remote logging API.
Process termination may prevent any final event; these logs are not a durable
audit store. Configuration lookup and post-hook validation occur outside the
wrapper and are not misreported as invocation success or failure.

A caller timeout or cancellation never releases a permit held by blocking
execution: the closure owns it until exit. A timed-out normalize operation must
not be used as authority to retry an upstream request. The wrapper does not
alter authentication, authorization, budget, SSRF, or non-idempotency checks.

CI contracts use isolated semaphores, explicit worker gates, a paused clock for
queue expiry, and a zero-deadline execution case accepting either worker-start
ordering. They verify no execution on denied capacity, distinct outcomes,
returned-value/error preservation, and retained capacity after cancellation or
timeout. Real-time five-second guards only prevent a broken test from hanging;
they do not establish correctness through a sleep margin. Local compilation,
production calls, and deployment are not part of this change.
