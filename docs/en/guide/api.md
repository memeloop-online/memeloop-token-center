# API overview

See the complete interface definition in [OpenAPI](https://github.com/memeloop-online/memeloop-token-center/blob/master/openapi/openapi.yaml). This page describes the conventions for authentication, versioning, pagination, and error handling.

## Surfaces and credentials

| Surface | Path | Credential |
| --- | --- | --- |
| Public gateway | `/v1/*` | `mtc_…` client credential |
| Self-service | `/self/v1/*`, `/portal` | `mtc_…` client credential (own data only) |
| Management | `/internal/v1/*`, `/operator`, `/version` | `mts_…` service credential, authorized by scope and tenant boundaries |
| Metrics | `/metrics` | Service credential with `metrics:read` |
| Probes | `/livez`, `/readyz` | None |

`/readyz` performs bounded, coalesced database and archive probes: a database failure returns 503; an archive failure returns 200 with a degraded marker, while asset operations remain fail-closed. `/healthz` is the deprecated alias for `/livez`; its response includes a deprecation warning header.

## Path groups

- `/v1`: OpenAI-compatible (`models`, `chat/completions`, `responses`, `embeddings`, `audio/transcriptions`), Anthropic-compatible (`messages`, `count_tokens`), and generation endpoints (`images`, `videos`, `generations`).
- `/self/v1`: `key`, `requests`, `stats`, `sessions`, `conversations`, `generations`, `usage-analysis`, `entitlements`.
- `/internal/v1`: `keys`, `service-tokens`, `upstreams`, `model-routes`, `provider-groups`, `route-groups`, `credential-groups`, `prices`, `requests`, `sessions`, `stats`, `usage-analysis`, `generations`, `plugins`, `plugin-runtime`, `tenants`, `schemas`, and more. Use `x-required-scope` in OpenAPI for the scope required by each path.

## Versioning and errors

- Responses from `/v1`, `/self/v1`, and `/internal/v1` carry `X-MTC-API-Version: v1`. The v1 policy allows additive changes (new fields and endpoints); removals require an announced deprecation window and a contract update.
- Errors return JSON with standard HTTP status codes and contain neither provider keys nor internal connection details.
- Oversized request bodies return 413 before JSON parsing; capacity saturation returns 503; a rate-limit policy match returns 429.

## Idempotency and concurrency

- Some write operations support or require the `Idempotency-Key` header; see the interface definition for the exact rule. With a key, an exact replay returns the original result, while a different body with the same key is rejected.
- Update endpoints use optimistic concurrency: send the `expected_updated_at`, `expected_version`, or revision obtained when reading. Conflicts return 409; read again and retry.

## Pagination and tenants

- Large historical lists use keyset cursors: `before_created_at` and `before_id` are supplied together, using the final row from the previous page. Page sizes are bounded (500 for request lists and 100 for typed queries).
- A global service credential selects a tenant with `tenant_external_id`; a tenant-bound credential is always restricted to its own tenant, and an out-of-bound access returns 403.

## Two Responses transports

`POST /v1/responses` is the HTTP Responses API; `GET /v1/responses` is its WebSocket form. Both use exactly the same authentication, route selection, reservation, settlement, archiving, and cancellation rules. WebSocket frame size and deadlines are bounded, and protocol errors are reported in safe error frames.
