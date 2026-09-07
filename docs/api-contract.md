# API contract and integration guide

The machine-readable contract is [openapi/openapi.yaml](../openapi/openapi.yaml).
This guide records endpoint invariants that need prose as well as schemas.

## Runtime surfaces

| Role | Routes | Credential |
| --- | --- | --- |
| gateway | `/v1/*`, `/self/v1/*`, `/portal` | Active `mtc_…` client credential |
| control | `/internal/v1/*`, `/operator`, `/metrics`, `/version` | Bootstrap or persisted `mts_…` service credential |
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
