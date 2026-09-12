# API contract and integration guide

The machine-readable contract is [openapi/openapi.yaml](../openapi/openapi.yaml).
This guide records endpoint invariants that need prose as well as schemas.

## Runtime surfaces

| Role | Routes | Credential |
| --- | --- | --- |
| gateway | `/v1/*`, `/self/v1/*`, `/portal` | Active `mtc_…` client credential |
| control | `/internal/v1/*`, `/operator`, `/version` | Bootstrap or persisted `mts_…` service credential |
| gateway, control, all | `/metrics` | Service credential with `metrics:read` |
| every HTTP role | `/livez`, `/readyz`, deprecated `/healthz`, `/ui-assets/*` | none |

Control is not exposed through public client ingress. `/livez` covers process
health. `/readyz` uses bounded, coalesced database and archive probes: database
failure returns 503; archive failure returns 200 with a degraded status while
asset operations remain fail-closed. `/healthz` is an alias for `/livez` and
advertises deprecation headers.

## Identity and authorization

`key_id` is the stable client-credential identity. Account, policy, usage,
conversations and balance survive credential rotation. `POST
`/internal/v1/keys/{key_id}/rotate` revokes the previous credential generation.
Client credentials cannot call administrative endpoints. Service credentials
have explicit scopes and tenant boundaries. Every object lookup checks the
credential and tenant boundary before returning data.

Administrative writes use `Idempotency-Key` where a retry could create or rotate
a resource. Exact replay returns the original result; reuse with a distinct
canonical request is rejected. Secret values are never returned after issuance.

## Provider accounts and routes

One provider account may use an API credential, native OAuth, plugin-provided
authorization or no credential. Its authentication method is metadata, not a
separate resource type. Inactive records remain readable for audit but cannot be
reactivated or routed without an explicit supported configuration.

OpenAI Codex account configuration always identifies the fixed official
`https://chatgpt.com/backend-api/codex` upstream; a SOCKS endpoint is transport
state inside the encrypted OAuth credential and is never represented as
`config.base_url`. Upstream responses expose only `has_proxy`, the SOCKS scheme,
remote-DNS semantics, a host-free label and a pepper-keyed fingerprint. They
never expose a proxy URL, host, port, username or password. The
`can_update_transport_proxy` capability is additionally restricted by the
authenticated caller and is always false for tenant-scoped services.

`PUT /internal/v1/upstreams/{account_id}/transport-proxy` is the only operation
that changes an existing Codex proxy. It requires a global `providers:write`
operator, exact tenant authorization, `Idempotency-Key`, and the current
`updated_at` and credential generation. It accepts only a private IP-literal
`socks5h` URL, retains all OAuth
material, rotates the encrypted credential generation atomically and writes a
secret-free audit record. A new Codex authorization requires the same write-only
`proxy_url`; reauthorization reuses the existing encrypted account proxy and
cannot replace it. The complete device lifecycle (user-code request, device
poll, token exchange and JWKS verification) and managed token refresh use that
same proxy with remote target DNS. Missing or invalid proxy state fails before
supplier DNS or network I/O, and there is no direct fallback. A Codex account
cannot be activated without an approved proxy, and the production Codex sender
independently fails closed before routing any unproxied request.

Codex `config.transport_policy` remains runtime-adjustable through the normal
upstream update CAS. Its bounded `connect_attempts`,
`connect_retry_delay_millis`, and `shared_probe_attempts` fields apply to newly
prepared inference requests without a service release; changing them never
changes the fixed destination or encrypted per-account proxy binding.

`GET /internal/v1/upstreams/{account_id}/deletion-readiness` reports the exact
lifecycle, direct-or-multi-candidate route, immutable history, and import
provenance blockers without mutating data. `DELETE` requires a disabled account
with none of those retained dependencies and repeats the check transactionally,
so a stale readiness read cannot delete a newly referenced upstream. Otherwise
the upstream remains disabled for audit and DELETE returns 409.

Model routes are tenant-scoped, versioned and optimistic-concurrency protected.
Route selection honors enabled state, grants, priority, health and bounded
round-robin behavior. A client may access only visible models. Historical request
attribution does not change when routes are disabled or replaced.

### Upstream account availability

Quota reset writes require `providers:write`, explicit tenant/account authorization
and schema 74. `POST /internal/v1/upstreams/{account_id}/quota-reset/prepare`
performs fresh supplier GETs and returns `{operation, confirmation_token}`.
The 120-second token binds actor, account, credential generation/transport revision
and the exact prepared credit counts. Prepare requires one `Idempotency-Key`; an
exact replay returns the same operation and derives the same confirmation token
without creating another operation or making another supplier read. The UI keeps
that token only in memory and may receive it again only by replaying the same
prepare key. The UI must name the account, supplier-defined
Codex rate limits and consumption of one reset credit; no specific 5h/weekly window
selection is supported by the supplier wire contract.
`POST .../quota-reset/{operation_id}/confirm` accepts only that token, the literal
`confirmation=consume_one_supplier_reset_credit`, and one `Idempotency-Key`; it
rechecks fresh credit counts and credential generation before an atomic dispatch claim.
An exact confirmation replay returns durable state and never dispatches again;
another key returns 409, while a missing or different explicit confirmation
literal is rejected before any claim. Transient refresh, local capacity, DNS, proxy,
or client-construction failures before that claim return 503 with `Retry-After` and
remain safe to retry using the same confirmation key.
One fixed-host POST follows the committed `submitted` state with retries and
redirects disabled. HTTP 2xx yields `accepted` (not proven quota recovery);
ambiguous outcomes are `unknown`. Both, and interrupted `submitted`, permanently
block further reset preparation pending separately reviewed manual handling.
`GET .../quota-reset/{operation_id}` reads durable state even after credential
revocation. `POST .../quota-reset/{operation_id}/reconcile` performs only supplier
GETs, records separately named observed counts and never changes the confirmation
baseline or infers success/unlocks a new consumption. No manual unlock endpoint
exists. Cancelling the confirmation dialog sends nothing; prepared operations
expire after 120 seconds. The supplier `redeem_request_id` is a correlation ID;
supplier idempotency has not been established and is never assumed.
All reset acceptance tests use mocks; production acceptance must not consume
credits without separate explicit authorization.
This is a fail-closed first-dispatch candidate, not a complete reusable reset
lifecycle. Evidence-backed operation closure and a separately authorized,
audited manual unlock workflow remain explicit follow-up work. Until that
protocol is implemented and reviewed, even `accepted` stays blocked and the UI
must say supplier acceptance is not proven consumption/recovery. Do not mark
reset delivery complete based on this candidate or its mocks alone.

Each reset response exposes preparation/confirmation timestamps and actors plus
the bounded durable event history (`prepared`, `confirmation_claimed`,
`dispatch_accepted|dispatch_unknown`, and `reconciled`). Raw idempotency keys,
confirmation tokens, OAuth material, supplier bodies, and proxy secrets are never
stored in that audit projection or returned.

`GET /internal/v1/upstreams/{account_id}/quota?tenant_external_id=...` requires
`providers:read` and an explicit authorized tenant. It returns the sanitized
`upstream_quota_v1` contract: provider/status, nullable observation and freshness
deadlines (epoch milliseconds), stale marker, plan, all applicable windows,
supplier credits, reset capability and closed product error codes. Window reset
times are epoch milliseconds; relative supplier offsets are marked estimated.
Unknown values stay null, never zero/unlimited. Supplier reset support is
distinct from MTC implementation availability and available/applicable credits.
This GET performs no reset, token refresh, probe or model invocation. Only
server-held credentials reach fixed native supplier GET endpoints; no upstream
body, email, account header, token or proxy secret is returned. The 30-second
cache is bounded to 128 identities, four concurrent account reads and one flight
per identity; each read has an eight-second deadline and 1 MiB response limit.
Read failures may retain explicitly stale observations for at most five minutes.
That stale last-known-good projection keeps reset integration available and marks
the failure retryable; a transient refresh error is not persisted as a permanent
loss of reset capability. Preparing is read-only and may retry the fresh check.
Other providers return unsupported rather than fabricated quota. All responses
use `Cache-Control: no-store`. Request quota only on explicit account inspection,
not one automatic request per row on page load.

A successful fresh Codex usage read may clear `quota_exhausted` only when the
supplier explicitly reports `allowed=true` for the code limit and does not
explicitly report `limit_reached=true`; an omitted `limit_reached` does not override
that affirmative allowed signal. The delete
is fenced by account credential generation, account status, the observation start
time, health update time, and any half-open lease. Cached/stale/failed/ambiguous
reads and newer 429 evidence never clear routing health.

`GET /internal/v1/upstream-availability` requires a service credential with both
`providers:read` and `requests:read`. All three query parameters are mandatory:
`tenant_external_id`, `from_created_at`, and `to_created_at`; unknown parameters
are rejected. The trimmed tenant selector must be non-empty and no more than
200 UTF-8 bytes. Even global credentials must explicitly select a tenant;
tenant-scoped credentials cannot select another tenant.

Time bounds are non-negative, inclusive Unix epoch milliseconds with start no
later than end. The difference must not exceed 93 days. Differences through
31 days use hourly rollups; longer windows use daily rollups. Partial edge
buckets read terminal request/generation facts to preserve the exact window.
Invalid selectors/windows return 400; missing authentication returns 401 and
insufficient scopes or a tenant-boundary violation returns 403.

The `Cache-Control: no-store` response is an
`UpstreamAccountAvailabilityWindow`, version `upstream_account_availability_v1`,
with generation time, resolved tenant, echoed bounds, granularity, latency
metadata, and `accounts`. It contains every current account for that tenant
(including disabled accounts), ordered by stable account ID, without pagination
or top-model truncation. Each account has `upstream_account_id`,
`MonitoringMetrics`, and at most five newest `MonitoringTerminalOutcome` entries
across all models. Outcomes retain their `request` or `generation` source and
original creation timestamp; in-flight traffic is excluded.

Accounts with no terminal traffic have zero counts, null average/p95 latency,
and empty costs/outcomes; no health status is inferred. Costs remain separate
by currency. `latency_is_approximate` is true and `latency_method` is
`fixed_histogram_upper_bound_capped_60000ms`: p95 is the fixed-histogram upper
bound capped at 60000 ms.

## Pricing and generation

Pricing is durable and uses model, currency and service tier. It records input,
cached input, cache write and output dimensions where available. Unspecified cache
dimensions use the input price and mark the estimate. Price synchronization is an
explicit control-plane action; manual values are preserved.

Text, image and video admission reserves applicable quota, balance and price
bounds. Terminal settlement records actual usage and releases unused reservation.
Async generation uses durable leases, idempotency and cancellation semantics.
Completed assets use authorized archive URLs rather than provider URLs.

## History and conversations

Request search accepts bounded time intervals, stable keyset cursors and exact
filters for credential, model, protocol, status, error, account, route, duration
and cost. Statistics use the same dimension rules and bounded intervals.

### Typed operator filters

`POST /internal/v1/requests/query` accepts a bounded, `and`-only typed-filter
AST. Fields, operators, and tagged value kinds are allow-listed; the query
adapter chooses fixed indexed columns and binds every value. It never accepts
SQL, a column name, an expression, or a raw query fragment. The endpoint remains
tenant/key scoped, limits pages to 100 rows, and uses a look-ahead keyset cursor.

Saved and recent filters are per service identity and tenant. The filter assistant
returns the same validated AST for an explicit browser preview; it cannot execute
model-generated SQL. A tenant administrator selects an enabled MTC model-route
reference and billing credential at `/internal/v1/filter-assistant/settings`.
Both references are revalidated before each request; normal proxy grants,
reservation, budgets, timeouts and accounting apply. Settings updates use
`expected_updated_at` and commit an audit receipt atomically. No credential
secret is restored or returned. Settings without a billing reference do not
invoke a model. Only the bounded user intent, current time and fixed AST schema
are sent; request records and upstream configuration are excluded. Invalid,
oversized or incomplete model output cannot become an applied filter.

Conversation APIs remain credential-scoped. They expose explicit session and
execution declarations, structured parent relations, bounded inferred edges and
confidence without inferring absent human or agent information. Archive detail may
report incomplete content when a bounded archive read cannot retrieve a body.

## Responses transports

`POST /v1/responses` is the HTTP Responses API. `GET /v1/responses` is its
authenticated WebSocket form. Both apply the same authentication, route selection,
reservation, settlement, archive and cancellation rules. Socket input and output
frames have bounded sizes and deadlines. WebSocket failures report a safe protocol
error; transport support is not withdrawn because an upstream has an incident.

## Versioning and errors

All `/internal`, `/self` and `/v1` responses include `X-MTC-API-Version: v1`.
The v1 policy permits additive changes. Removal requires a documented deprecation
window, an OpenAPI update and conformance checks. Errors use a sanitized stable
envelope and must not disclose provider secrets, archive locators or internal
network topology.

The release process verifies the OpenAPI route boundary table, schema references
and version metadata in GitHub Actions. Migration generations are discovered from
the registered migration set rather than copied into this document.
