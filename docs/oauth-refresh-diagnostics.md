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
