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

## Kimi timeout and recovery boundary

Kimi refresh has an unchanged ten-second request and operation budget. The
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
