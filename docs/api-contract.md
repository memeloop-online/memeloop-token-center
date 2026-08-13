# API contract and integration guide

The machine-readable contract is [`../openapi/openapi.yaml`](../openapi/openapi.yaml). It describes the versioned API routes registered by `src/api.rs`, operational probes/metadata, and the public `/portal` shell; static `/operator` and `/ui-assets/*` delivery is outside the API contract. This guide documents the cross-endpoint invariants that OpenAPI alone cannot express.

## Runtime surfaces

Production deployments separate two HTTP roles:

| Role | Routes | Credential accepted |
|---|---|---|
| `gateway` | `/v1/*`, `/self/v1/*`, `/portal` | Native `mtc_…` or an active migrated CPA client credential |
| `control` | `/internal/v1/*`, `/operator`, `/metrics`, `/version` | Bootstrap service token or persisted `mts_…` service credential; operational metadata requires `metrics:read` |
| every HTTP role | `/livez`, `/readyz`, deprecated `/healthz`, `/ui-assets/*` | none |

Do not expose the control service through the public client ingress. `/livez` proves only process liveness. `/readyz` checks PostgreSQL and the configured archive with bounded, coalesced dependency probes and returns 503 while either is unavailable. `/healthz` is a deprecated compatibility alias for `/livez` and advertises its successor in response headers. Gateway and worker roles do not register `/metrics` or `/version`.

## Stable identities and secrets

`key_id` is the stable client-credential identity. `account_id`, policy, usage history, logical conversations and balance survive rotation. The secret returned as `key` is only one generation of that identity. `POST /internal/v1/keys/{key_id}/rotate` immediately revokes previous native and legacy credential generations.

`service_id` has the same stability property for service credentials. A service create or rotate response contains the only returned copy of `token`; list operations expose only a fingerprint.

Client, service and unified-upstream credential rotation—and upstream OAuth refresh—require `Idempotency-Key` in one global rotation namespace. Replaying the same key for the same normalized resource operation within 24 hours returns the stored generation/result; client and service replay includes the same encrypted one-time secret. Reuse for another resource or different operation is rejected. This makes an ambiguous rotation/refresh timeout recoverable without accidentally revoking the just-issued generation or refreshing twice.

Status transitions are:

- `active → suspended → active` is reversible.
- `active|suspended → revoked` is terminal.
- Revocation invalidates every current secret and cannot be reversed by the API.

Callers must never use a credential secret as their database primary key. Persist the stable UUID and fingerprint, and keep the one-time secret in an appropriate secret store.

## Tenant boundary and scopes

A service credential is either global (`tenant_external_id = null`) or bound to one external tenant ID. Tenant-bound credentials cannot select, create or mutate a different tenant. A global credential may omit `tenant_external_id` on supported list/statistics endpoints to aggregate all tenants.

Recommended memeloop web scopes are:

| Responsibility | Minimum scopes |
|---|---|
| register/reconcile a user credential | `keys:read`, `keys:write` |
| reconcile subscription entitlement and inspect the ledger | `entitlements:read`, `entitlements:write`, `credits:read` |
| render tenant traffic/support views | `requests:read` |

Bind that service credential to the memeloop web tenant. Do not give web registration traffic `service_tokens:*`, `prices:*`, `providers:*`, `routes:*`, or `oauth:write`.

## Registration and reconciliation

The recommended registration transaction is:

1. Build a deterministic `Idempotency-Key` from the memeloop tenant, user ID and provisioning version.
2. `POST /internal/v1/keys` with the external tenant/user IDs, an explicit policy and initial balance.
3. Persist `key_id`, `account_id`, `credential_generation`, fingerprint and the one-time secret before acknowledging registration.
4. On an ambiguous timeout, replay the identical request with the same idempotency key.
5. For later reconciliation, call `GET /internal/v1/keys?principal_external_id=…`; a tenant-bound service credential is confined to its tenant automatically.

The encrypted idempotent provisioning response is retained for 24 hours. After expiry, metadata remains discoverable through the list API, but the original secret cannot be recovered. Rotate the stable identity if the secret was lost.

## Subscription credit lifecycle

For subscription automation, use the exact entitlement resource rather than additive grants. Its stable identity is `(tenant, provider, external_subscription_id)` and each billing cycle has its own stable `external_cycle_id`. `PUT /internal/v1/entitlements` accepts a desired amount and monotonically increasing external version. It atomically adjusts the account, persists the current cycle, and returns `desired`, FIFO-attributed `consumed`, withdrawable `remaining`, and the exact `ledger_delta`.

Every entitlement write requires `Idempotency-Key`; its replay namespace is the tenant. The identical canonical operation returns the original response, while the same key with another payload returns HTTP 409 `conflict`. A successful non-replay update must have a version greater than the current stable subscription version.

Cancellation withdraws only the unconsumed remainder. Historical `desired` and `consumed` values remain visible, `remaining` becomes zero, and later reconciliation can reactivate the same stable subscription only with an explicitly higher version. Replacement atomically withdraws the old remainder, marks the old identity `replaced`, and creates a different provider/subscription identity on the same tenant and credit account; a replaced identity cannot be revived. Settled usage is attributed FIFO to entitlement cycles that were active for that account, which makes later cancellation and replacement independent of unrelated account credit.

The older grant API remains available for non-subscription adjustments. Credit grants require `Idempotency-Key` and a positive decimal `amount`; `POST /internal/v1/accounts/{account_id}/grant-reversals` can reverse only an entire grant when the account has no later usage. It is not a substitute for entitlement reconciliation.

Use `GET /internal/v1/accounts/{account_id}/ledger` to reconcile recent grant, reversal and usage records. It currently returns at most 500 rows and has no cursor.

## Policy semantics

Policy replacement is whole-resource `PUT`, not a merge patch. Send every field that must be retained.

- `allowed_models: []` denies every model.
- `allowed_models: ["*"]` permits every priced and routed model.
- RPM, TPM and maximum concurrency must be positive.
- Daily, rolling-weekly and lifetime budgets are non-negative decimal strings in the credential currency, or `null` for no limit.

Balance and budgets are checked before upstream execution. Error code `insufficient_quota` and rate/concurrency error code `rate_limit_exceeded` both currently use HTTP 429.

## Unified upstream providers

An upstream account is one resource regardless of whether its authentication method is API credential, OAuth, CPA subscription bridge or no authentication. `auth_kind` is metadata about the current method; it is not a separate account type.

Direct credentials use `POST /internal/v1/upstreams`. OAuth start/poll endpoints create the same upstream resource, and routes always reference its stable `account_id`. The unified list exposes `connection_method`, `credential_expires_at` and `route_count` without returning authentication material.

`PUT /internal/v1/upstreams/{account_id}` replaces the editable name/configuration, while `PATCH` changes only `active|disabled`; both use `expected_updated_at` optimistic concurrency and safely replay an already-applied target state. `POST .../health` performs a bounded provider-specific read, drops the upstream body and returns only sanitized status/error/latency metadata. `DELETE` is deliberately narrow: an account must be disabled and have neither routes nor text/generation history. Otherwise it remains disabled for audit and the API returns 409.

Both `PUT .../credential` and `POST .../oauth/refresh` require `Idempotency-Key`. A successful replay within 24 hours returns the stored credential generation, while reuse for another stable resource or different rotation is rejected. OAuth refresh claims the key before calling the authorization server so an ambiguous timeout cannot trigger a second refresh. Neither operation changes `account_id`, route attribution or history.

Provider and credential JSON Schemas are discoverable through `/internal/v1/provider-types`. A plugin-contributed provider may add an OAuth adapter without receiving arbitrary in-process authority.

Routes use optimistic concurrency. `PUT` replaces routing fields, `PATCH` changes enabled state, and both accept the most recently read `updated_at`; a stale version for a real change returns 409 while replaying already-applied target values is safe. `DELETE` requires the route to be disabled and unreferenced by request history, preserving historical route attribution. Tenant-scoped services cannot select or mutate a route or upstream outside their bound tenant.

## Pricing and multimodal generation

Token price synchronization is an explicit operator action. It fetches only the server-owned models.dev, LiteLLM and OpenRouter URLs in that preference order, preserves manual prices and the last-known preferred-source value during a source outage, saves unambiguous matches and returns ambiguous candidates without automatically selecting one.

Each token price is keyed by `(model, currency, service_tier)` and carries four charge dimensions: uncached input, cached input, cache write and output. Supported tiers are `default`, `auto`, `priority`, `flex`, `scale`, `batch` and `standard_only`. A manual or catalog price that omits a cache dimension conservatively uses the uncached input price and marks `cache_price_estimated: true`. Reservation snapshots capture all available tiers; settlement uses normalized OpenAI/Anthropic cache usage and the reported tier. An unknown reported tier is charged against the most expensive snapshotted tier instead of silently undercharging.

Multimodal prices use a `billing_unit` (`job`, `second`, `image`, or `megapixel`). Driver execution is currently stricter: asynchronous Seedance requires `second`, while asynchronous ComfyUI requires `job`. OpenAI-compatible Images and Codex Responses image-tool routes are synchronous `/v1/images/generations` paths and share the same permission, reservation, settlement and archive pipeline.

The asynchronous form of `POST /v1/generations`, `POST /v1/videos/generations`, and `POST /v1/images/generations` accepts an optional `Idempotency-Key`, scoped to the authenticated stable `key_id`. The first submission returns 202. Replaying the key with the same canonical `model` and `input` returns the same job with 200 and neither creates a second job nor retains a duplicate balance reservation. Reusing the key with different content returns 400. The header does not currently make the synchronous OpenAI-compatible Images path replayable.

`DELETE /self/v1/generations/{job_id}` atomically cancels and refunds only a queued job that has not acquired an active worker lease. Repeating cancellation of an already-cancelled job is safe. Running or terminal jobs require provider-specific cancellation support and currently return 400.

## Traffic, archives, and logical conversations

The authenticated client self-service surface cannot cross `key_id`. The operator surface additionally enforces service scope and tenant boundary.

`/internal/v1/request-events` uses the ordered pair `(after_event_at, after_event_id)` as its resume cursor. Operator request lists support inclusive `from_created_at`/`to_created_at` bounds; the exclusive `(before_created_at, before_id)` descending keyset cursor; exact `key_id`, `model`, `protocol`, logical `status`, `error_code`, `upstream_account_id` and `route_id`; inclusive duration/cost ranges; and case-insensitive literal-prefix searches for credential alias and principal. All supplied filters are combined with AND. The next page cursor is the last returned row's `(created_at, request_id)`. Self-service exposes every applicable time/model/protocol/status/error/upstream/route/duration/cost filter but always binds the authenticated `key_id`; alias and principal search remain operator-only. The account-ledger list still has no cursor.

Operator, self-service and price-usage statistics reuse the same applicable dimensions but do not perform an all-time raw-table query. Omitted `to_created_at` means request time; omitted `from_created_at` means 30 days before the effective upper bound. The resulting inclusive interval may be at most 93 days. This query bound is not a claim that request records or archive objects are retained for only 93 days—storage retention remains an independent operational policy.

After CPAMP metadata import, the separate `import-cpa-session-archive` command can rehydrate schema-v2 JSONL request/response bodies into the target BLAKE3 CAS and add supported conversation observations. It is dry-run/fail-closed by default, requires exact tenant/source/request/time/model/full-key-hash mapping, maintains tenant/source checkpoints with overlap, and refuses to replace a non-`gap://` object. The source format does not contain upstream request/response ancestry IDs; missing strong ancestry evidence is therefore left unconnected rather than fabricated. See `docs/session-archive-import.md` for the operational flow.

Schema v16 stores the successful upstream OpenAI Responses `id` on its conversation observation. A later `/v1/responses` request whose `previous_response_id` matches that stored ID is treated as direct parent evidence. This preserves the logical conversation even when the client supplies no Token Center-specific session or turn headers. The stored upstream ID is tenant/principal constrained by the conversation lookup and is not exposed as an administrative identifier.

Archive detail is bounded and may return `archive_complete: false` if an archived body is absent or exceeds the read bound. Consumers must not treat an incomplete archive as an empty upstream response.

## Idempotency and errors

All errors use:

```json
{
  "error": {
    "code": "invalid_request",
    "message": "invalid request: …"
  }
}
```

Authentication is evaluated before protected request bodies are buffered. API responses include `Cache-Control: no-store` and an `X-MTC-Request-Id` correlation header. Upstream and storage error detail is logged server-side and is sanitized in the HTTP response.

Use a distinct `Idempotency-Key` for each logical create, rotation, asynchronous generation, entitlement operation, grant, and reversal. Reuse is valid only for replaying the same normalized operation. Client provisioning and asynchronous generation reject reuse with different request content; rotation additionally rejects reuse across stable resources; entitlement reuse with another canonical payload returns HTTP 409 rather than 400.

## Compatibility and versioning

The route prefix is currently `v1`; the application and this contract are `0.1.0`, and the current working-tree database schema is v23. A fresh restricted PostgreSQL database applied exactly schema migrations 1–23 with `missing=0`; the PostgreSQL DB regressions passed 3/3, `tests/postgres.rs` passed 3/3 including v22 budget concurrency, `entitlements_postgres` passed 1/1, and `observability_filters` passed 2/2. This functional gate does not replace the pending 141k-row migration lock-time or imported-scale EXPLAIN/latency evidence. Control-plane callers with `metrics:read` can inspect `/version`, which reports build revision/target/timestamp, the current and supported API versions, compatibility text, and deprecated paths. All `/internal`, `/self`, and `/v1` responses carry `X-MTC-API-Version: v1`. The stated v1 policy permits additive changes; removals require a documented deprecation window. A release process must still update the OpenAPI version and run lint/conformance checks whenever a request, response, scope, or status code changes.

## Operational metrics

`/metrics` is registered only on control/all roles and requires a service credential with `metrics:read`. It exports build identity, dependency readiness, bounded HTTP and upstream request/error/duration series, database pool state, and queued/running generation counts. Labels intentionally exclude tenant, credential, model, concrete URL/resource ID, and error text to prevent secrets and unbounded cardinality from entering monitoring storage.
