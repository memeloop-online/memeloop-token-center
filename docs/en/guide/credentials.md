# Credential management

MTC has two completely isolated credential types.

| Type | Prefix | Callable interfaces | Creation |
| --- | --- | --- | --- |
| Client credential | `mtc_…` | Authorized `/v1/*` model interfaces and its own `/self/v1/*` self-service views | `POST /internal/v1/keys` or the Operator console |
| Service credential | `mts_…` | Scope-authorized `/internal/v1/*` management interfaces | `POST /internal/v1/service-tokens` or the Operator console |

Client credentials can never call management interfaces; service credentials cannot substitute for a client credential when making model requests.

## Stable identity and rotation

- Each client credential has an immutable UUIDv7 `key_id`, the stable owner of its billing account, policies, request history, statistics, and session data.
- Rotation (`POST /internal/v1/keys/{key_id}/rotate`) invalidates the old credential and issues a new one; the `key_id` and all history remain unchanged.
- The rotation endpoint requires `Idempotency-Key`, returns the new credential with `Cache-Control: no-store`, and still allows it to be retrieved later through the copy operation.

## Copying a credential

Click **Copy credential** in the client credential list to copy its currently usable value. An integrated management interface can call:

```bash
curl -X POST "https://mtc.example.com/internal/v1/keys/0193f2ab-7c1e-7000-8000-0000000000c3/credential-recovery/copy?tenant_external_id=default" \
  -H "Authorization: Bearer mts_example_service_token"
```

- Requires the `keys:write` scope. The response is `no-store` and includes plaintext only while the credential remains usable.
- Repeated calls return the same current credential and trigger no rotation or other change.
- Lists and self-service interfaces never include plaintext; a credential that cannot be copied can only be replaced through rotation.

## Creating a client credential

Common fields for `POST /internal/v1/keys` (see the complete definition in the [key-create JSON Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/key-create.schema.json)):

| Field | Description |
| --- | --- |
| `principal_external_id` | Stable identifier for the calling principal (required) |
| `alias` | Management-friendly alias (required) |
| `currency` | `USD` or `CNY`, default `USD` |
| `initial_balance` | Initial prepaid balance as a decimal string |
| `route_ids` / `route_group_ids` | Granted model routes / route groups; an empty array grants no models |
| `policy` | Rate-limit and budget policy, described below |

`policy` fields:

| Field | Default | Description |
| --- | --- | --- |
| `requests_per_minute` | 60 | Maximum requests per minute |
| `tokens_per_minute` | 100000 | Maximum tokens per minute |
| `max_concurrency` | 4 | Maximum concurrent requests |
| `enforcement_mode` | `prepaid` | `prepaid` enforces balance and limits synchronously; `metered_unlimited` performs exact postpaid accounting without shared admission limits |
| `daily_budget` / `weekly_budget` / `lifetime_budget` | None | Budget caps; reaching one causes rejection |

After creation, update individual sections through `PUT /internal/v1/keys/{key_id}/policy`, `/limits`, `/routing`, `/status`, and `/alias`. Endpoints requiring concurrency control identify the version field to send in OpenAPI; follow that definition.

## Service credentials and scopes

`POST /internal/v1/service-tokens` accepts `name`, a `scopes` array, and optional `tenant_external_id` (empty means a global operator credential; a value restricts the credential to one tenant). Grantable scopes are grouped below; see the [service-token JSON Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/service-token.schema.json) and each endpoint's `x-required-scope` annotation:

- Credentials and funds: `keys:read`, `keys:write`, `credits:read`, `credits:write`, `entitlements:read`, `entitlements:write`, `settlements:adjust`
- Traffic and generation: `requests:read`, `generations:write`, `generations:quarantine:read`, `generations:reconcile`, `filter_assistant:execute`
- Upstreams and routing: `providers:read`, `providers:write`, `oauth:write`, `routes:read`, `routes:write`, `upstreams:import:write`
- Plugins and pricing: `plugins:read`, `plugins:write`, `prices:read`, `prices:write`
- System: `service_tokens:read`, `service_tokens:write`, `tenants:read`, `tenants:write`, `schemas:read`, `metrics:read`

Note: `filter_assistant:execute` is a separate billable execution permission; `requests:read` does not imply it. Service credentials also support the `rotate`, `copy`, and `status` management interfaces.

## Write idempotency

Some write operations support or require `Idempotency-Key`; the OpenAPI annotation is authoritative. Rotation and balance top-ups require it, while credential and route creation may include it optionally. An exact replay with the same key returns the original result; a different body with that key is rejected.

## Client self-service views

A client holding an `mtc_…` credential can query:

| Interface | Content |
| --- | --- |
| `GET /self/v1/key` | Its credential information (without plaintext) |
| `GET /self/v1/key/limits` | Current rate-limit and budget snapshot |
| `GET /self/v1/requests`, `/stats`, `/sessions`, `/conversations`, `/generations`, `/usage-analysis`, `/entitlements` | Its own history and usage |

Browser users can use the `/portal` self-service portal. Once a credential is entered, the data scope remains limited to that credential.
