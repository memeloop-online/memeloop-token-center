# Optional credential rate limits

Prepaid credentials still require sufficient credit for the trusted reservation,
atomically hold that money, and settle actual usage idempotently. An unset rate
limit does not change identity, route authorization, balance, or budget checks.

`requests_per_minute`, `tokens_per_minute`, and `max_concurrency` are independently
optional. Omission or JSON `null` means unconfigured; a positive integer enables
the corresponding limit. Zero is invalid, not an infinity sentinel. New
credentials have no rate or concurrency limits unless explicitly configured.

Both `POST /internal/v1/keys` and `PUT /internal/v1/keys/{key_id}/policy` accept this
policy. PUT replaces the whole policy, as before; it is not a partial update.
Existing stored numeric limits are preserved, including old numeric defaults:
there is no safe way to distinguish those from a user's explicit choice. Clear
them explicitly with `null` when the credential should be unrestricted by rates.
Do not switch to `metered_unlimited` just to remove prepaid rate limits.

The published `key_create` and `key_policy` schemas from
`GET /internal/v1/schemas` accept nullable rate fields and have no numeric rate
defaults. Forms should show an unset choice rather than inject a large number.

`GET /self/v1/key/limits` returns `null` for an unconfigured rate's `limit`,
`remaining`, and `reset_at`, and for unconfigured concurrency's `limit` and
`remaining`. `used` and `active` stay numeric. Rate-window counters are admission
accounting for configured rate limits, not an all-traffic usage meter.

Admission does not read/write a rate window when neither RPM nor applicable TPM
is configured, and does not count active reservations when concurrency is unset.
Configured rates reuse the existing single window and atomic bounds; only enabled
RPM/TPM contribute to their respective counters. Financial
key/account locking remains per key/account; no new shared rate lock is added.

Migration 134 adds TPM-window ownership metadata to existing usage reservations.
Historic records default to owning their original token reservation. New reservations
that did not use TPM keep that fact across resizing, cancellation, recovery,
and settlement, even if the credential's policy changes in the meantime. They
must never subtract tokens from a later finite request's window.

## Failure attribution

Configured credential RPM/TPM/concurrency rejection is HTTP 429 with
`rate_limit_exceeded` and the matching `reason`; insufficient prepaid money is
429 with `insufficient_quota`. Neither is a generic 503.

A 503 must be attributed using the request's typed error and admission/dispatch
evidence: `no_authorized_upstream_candidate` is local candidate rejection;
`database_unavailable` and `database_storage_full` are database dependency
failures. `service_overloaded` is not enough to identify the cause: a database
pool timeout also uses that public code, with `error_kind=pool_timeout` in the
existing database error log. Existing memory/dispatch rejection must be
distinguished from a request actually sent to an upstream that replied 503.
Do not infer a credential rate limit or add a substitute cap from status alone.
