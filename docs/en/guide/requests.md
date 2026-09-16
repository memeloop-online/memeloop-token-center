# Requests and usage

MTC preserves stable tenant, credential, route, account, and price-snapshot ownership for every request. This page explains how to query history, attach session metadata to requests, and understand usage and cost semantics.

## Query interfaces

| Role | Interface | Content |
| --- | --- | --- |
| Operator (`requests:read`) | `GET /internal/v1/requests` | Recent requests and generation jobs, up to 500 rows |
| Operator | `GET /internal/v1/requests/{request_id}` | Single-request details (include `request_kind` as `text` or `generation`) |
| Operator | `GET /internal/v1/requests/{request_id}/archive/{side}` | Archived body (`side` is `request` or `response`) |
| Operator | `GET /internal/v1/stats`, `/usage-analysis`, `/request-events`, `/sessions` | Aggregate statistics, usage analysis, live events, and session views |
| Client | `GET /self/v1/requests`, `/stats`, `/sessions`, `/conversations` | The same semantics, limited to the client's credential |

Pagination uses a keyset cursor: take the previous page's final `created_at` and `request_id` and send them as `before_created_at` + `before_id` (both are required). List filters combine with AND and time ranges are inclusive; an exact single-item query cannot be combined with cursor parameters.

## Typed filters

`POST /internal/v1/requests/query` accepts a typed filter tree with AND only. Fields, operators, and value types are allowlisted; the server maps them to fixed indexed columns and binds parameters. It is not SQL and cannot execute arbitrary query text. The limit is 12 conditions and 100 rows.

```json
{
  "tenant_external_id": "default",
  "limit": 50,
  "ast": {
    "logical_operator": "and",
    "conditions": [
      { "field": "model", "operator": "equals", "value": { "type": "model", "value": "example-chat" } },
      { "field": "status", "operator": "equals", "value": { "type": "status", "value": "error" } },
      { "field": "created_at", "operator": "greater_than_or_equal", "value": { "type": "timestamp", "value": 1780000000000 } }
    ]
  }
}
```

Available fields: `created_at`, `key_id`, `model`, `protocol`, `status`, `error_code`, `upstream_account_id`, `route_id`, `duration_ms`, `cost_micros`, `key_alias`, and `principal`. Saved and recent filters (`/internal/v1/filter-presets`) are isolated by service identity and tenant.

### Filter assistant

Operators can have a model translate a natural-language intent into the filter tree above. After `PUT /internal/v1/filter-assistant/settings` configures an enabled MTC model route and billing credential, `POST /internal/v1/filter-assistant/plan` returns a validated AST for preview. Only the user's intent, current time, and fixed AST structure are sent to the model; request records and upstream configuration are never sent out. Model calls require the separate `filter_assistant:execute` scope, and invalid or out-of-bounds model output never becomes an applied filter.

## Session and execution metadata

Downstream applications can attach optional declarative metadata to any `/v1/*` text request for timeline, relationship-graph, and cost views in the console:

| Header | Meaning |
| --- | --- |
| `X-MTC-Session-Name` | Human-readable session name |
| `traceparent` | W3C trace context |
| `X-MTC-Trace-Id` / `X-MTC-Span-Id` / `X-MTC-Parent-Span-Id` | Explicit trace/span overrides |
| `X-MTC-Agent-Id` / `X-MTC-Parent-Agent-Id` | Stable agent instance/role and its parent |
| `X-MTC-Task-Kind` | Task type, such as `interactive` or `background` |
| `X-MTC-Session-Labels` | JSON object of at most 16 string label pairs |

Key points:

- Equivalent fields may also be placed under `metadata` in the request body (snake_case); headers take precedence. Label keys are at most 64 characters and values at most 128; keys resembling credentials or secrets are discarded.
- These declarations are for visualization and audit projections only. They do not change authorization, routing, or billing identity, and do not upgrade low-confidence candidate relationships into confirmed relationships.
- Missing values remain missing—MTC does not use a model to “guess” a session name or task type from the prompt. The `structure` projection in session details separately carries protocol evidence (session/turn/parent/response IDs, and so on) and is clearly distinguished from human declarations.

## Usage and cost semantics

The `usage_basis` in a request record identifies the source of token usage:

| Value | Meaning |
| --- | --- |
| `provider_reported` | Measured usage reported by the provider at terminal state |
| `provider_estimated` | Estimate supplied by the provider |
| `contract_ceiling` | Conservative settlement at the contract ceiling when reliable usage was unavailable |
| `not_observed` | No valid usage observed; local measured usage is zero |

When interpreting cost:

- `cost` is the settlement amount in MTC's local ledger, not the provider invoice; reconcile external billing separately.
- `not_observed` means “cost unknown,” not “cost is zero”: a request record must not treat numeric zero as the actual zero cost.
- Only output tokens with `provider_reported` usage can be used as the measured generation-speed numerator.

## Archive completeness

Request and response bodies enter the archive after encryption. `archive_complete=false` in details means the body is not yet complete; use archive status to distinguish waiting for upload, upload failure, or missing data. The field itself does not guarantee that the body will later be readable. Generation artifacts are read through `GET …/generations/{job_id}/assets/{asset_id}` or the request asset interface.
