# OAuth refresh diagnostics

Control-plane builds with migration 120 add optional `oauth_refresh` metadata
to `GET /internal/v1/upstreams`. The existing `providers:read` permission,
tenant selection and pagination apply unchanged. The additional projection
uses one bounded query for the selected page; it reads credential expiry,
revocation, refresh timestamps and the presence of a saved result, not token
contents. If the diagnostic query fails or times out, accounts remain readable
and the optional field is omitted.

The projection provides:

- `credential_generation` and `access_state`: `valid`, `expired`, `revoked`
  or `unknown`. A future access expiry does not establish overall health.
- `refresh_state`: `not_observed`, `in_progress`, `pending_local`,
  `outcome_unknown`, `failed` or `reauthorization_required`.
- Optional attempt timestamps, an allowlisted `failure_class`, and numeric
  `http_status`. No remote description, response body, URL, credential or
  idempotency key is returned.
- `reauthorization_required`, true only when a new refresh attempt has an
  explicit supported rejection such as `invalid_grant`. Access expiry,
  generic HTTP 401/403, and historical unknown attempts do not establish this.
- `automatic_refresh_paused`, true when the current generation has a started
  attempt with no saved result and its refresh state is `outcome_unknown` or
  `reauthorization_required`. This is an observation of the existing replay
  protection, not a new scheduling gate or proof of invalid authorization.
  Active attempts, saved results awaiting local completion, and access expiry
  alone do not set it. Clients predating this field can ignore it.

Administrative `status`, `can_refresh` and `can_reauthorize` keep their existing
meanings. Diagnostics do not affect routing, model visibility, availability
admission or refresh scheduling. `can_refresh` describes lifecycle capability,
not a guarantee that an individual refresh will succeed.

Suggested user-facing interpretation:

- `outcome_unknown`: “The authorization refresh result could not be confirmed.
  You can authorize this account again to restore access.” This is a recovery
  option, not proof that its previous refresh token was invalid.
- `reauthorization_required`: “Authorize this account again to restore automatic
  authorization updates.”
- `pending_local`: “An authorization update is awaiting completion. Contact
  your administrator if it does not complete.” Do not initiate another remote
  refresh to complete the saved update.
- `not_observed`: “No refresh result has been recorded.” Do not label this as
  a successful or healthy refresh.

Migration 120 only adds nullable `upstream_credentials.oauth_refresh_diagnostic_json`
metadata with a length bound. Both database registries and the canonical chart
schema version advance together. Existing migration 119 and its data are not
modified. Existing unknown attempts are not backfilled with invented failures.

Only failure branches of the shared worker/control refresh operation write
diagnostics, best-effort with a two-second bound. Generation and existing
attempt identity checks prevent stale attempts from replacing current
metadata. Unsent cleanup, started-request protection, saved-result staging,
finalization and exact replay retain their previous semantics. Successful
refresh staging does not depend on diagnostic storage. Diagnostic writes do
not run in the inference forwarding or candidate-resolution paths.

The stored diagnostic is a per-generation latest failure snapshot, not an
immutable history of every attempt. Kimi errors are typed without logging
provider response content; unsupported provider error formats remain unknown.
This change does not recover or reauthorize existing production accounts.

New Kimi network failures optionally include a bounded `failure.observation`
in the existing durable diagnostic, projected as `failure_observation`:
`phase` (`before_headers` or `response_body`), monotonic `elapsed_ms`,
`budget_ms`, `connector_failure`, and `timeout_source` (`http_client`,
`operation_deadline`, or null). `before_headers` means the HTTP execute future
has not yielded response headers; it does not prove the POST was delivered or
identify a DNS/TLS phase. Body-reader failures do not invent a timeout source.
The observation starts after the durable dispatch marker completes, not when
the lease is claimed. No URL, error string, token or response body is retained.

Old diagnostics remain readable without invented observations. Older binaries
with strict diagnostic decoders ignore diagnostics containing the new optional
field; deploy the reader alongside the writer to retain detailed presentation.
Replay protection uses lease/pending state, not these optional observations.
This addition does not change that protection or restore a lost remote result.

## Kimi timeout and recovery boundary

Kimi refresh has a 120-second request and operation budget, replacing the
previous ten-second cutoff. The
durable `request_started_at` marker is written before executing the request:
it is a replay-safety boundary, not proof that the provider received a POST.
A timeout can occur while connecting, awaiting headers, or reading the body.
New body-deadline diagnostics preserve an already observed HTTP status; a
missing status does not establish a particular network failure phase for
historical attempts. A status without a complete response does not establish
that a replacement credential was received or that refresh succeeded.

The worker checks expiring credentials every minute, five minutes ahead of
expiry. It excludes a generation with a started request and no saved result,
even after lease expiry. Only undispatched expired leases can be reclaimed;
saved encrypted results can be finalized locally without another remote POST.
Without provider-confirmed idempotency or token-reuse semantics, do not replay
an unknown refresh, clear its lease, or infer `invalid_grant` from access expiry.
An operator alert should say that automatic refresh is paused because its
result is unconfirmed, not that the user failed to sign in. Do not automatically
initiate authorization. A later user-supplied authorization supersedes the old
generation; its healthy state is not evidence that the timeout cause was fixed.

## Kimi prevention and bounded recovery

The official Kimi CLI at revision
[`9ab1286b`](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/src/kimi_cli/utils/aiohttp.py)
uses a 120-second total budget, 60-second socket-read budget and 15-second
connection budget. MTC adopts the total budget for refresh only; its existing
five-second connector limit and shared TLS/proxy validation remain unchanged.
This avoids discarding a valid exchange solely because headers or the complete
body take longer than ten seconds. The byte bound still applies.

The [official refresh implementation](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/src/kimi_cli/auth/oauth.py)
retries transient errors, coordinates local processes with a file lock, reloads
persisted tokens and atomically replaces the credential file on success. This
is client behavior, not a server guarantee of refresh-token reuse or remote
idempotency. MTC does not copy the ambiguous POST retry behavior.

Only a connector-classified failure from the fresh no-retry Kimi HTTP client
can undo its own dispatch marker. The transition locks the account and matches
the claimed credential generation and idempotency key; a pending result or
superseding credential prevents release. Normal cleanup then makes that
unconsumed credential eligible for the next worker pass. Failure to persist
this transition leaves the marker in place. An ordinary total timeout, a
received HTTP rejection or a disconnect after POST does not release it.

All managed OAuth providers use the same local result installation path. A
complete result is sealed once and retained for up to seven local staging and
finalization attempts, with 100ms, 500ms, 1s, 2s, 4s and 8s backoff. Transient
database unavailability and pool exhaustion join storage/internal failures in
this bounded local retry. Confirmed storage-full errors, claim conflicts and
provider rejections are not retried by this loop. Every attempt reuses the same
ciphertext and generation/claim fences, and never calls the provider. An
exhausted failure emits a secret-free error with account ID, generation, stage
and error category so it can be alerted separately from remote rejection.

The existing worker resumes a saved pending result using its original persisted
idempotency key, including results from manual refresh or worker takeover. It
does not manufacture a new key to retry a saved result. Before a new remote
exchange, its selected generation must still match the locked account row;
manual authorization installed after selection is preserved. Unknown remote
outcomes without saved results remain excluded from automatic scheduling.

These are bounded protections, not a guarantee against losing an unstaged
response during a process crash or a prolonged database outage. Already staged
results remain durable and use the existing local replay path. Historical
unknown POSTs stay blocked; no production lease or credential is rewritten by
deployment of this change.

On worker shutdown, ordinary roles retain their 30-second shared grace. The
single OAuth-refresh role stops scheduling new work and retains up to180seconds
to finish the current exchange and local persistence. Both task sets remain
owned and joined; the OAuth lane is forcibly cancelled with a safe error event
only at its own bounded deadline. In `all` mode, process shutdown starts this
OAuth drain concurrently with HTTP connection drain, not after it. Ordinary
worker roles remain available for in-flight HTTP cleanup until HTTP drain
finishes, then retain their original 30-second grace. Finishing HTTP drain does
not restart the OAuth deadline. The existing chart pod termination grace of
1320seconds accommodates the default 1260second HTTP budget and these concurrent
drains; neither HTTP grace nor pod settings are changed. A process kill
or deployment overriding the pod grace below this drain budget can still lose
an unstaged result.
