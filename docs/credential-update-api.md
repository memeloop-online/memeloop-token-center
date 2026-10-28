# Credential update API handoff

Backend candidate based on master `5aaa45e487bb71d287b1fd51e14be442a086abb5`.
UI owner: `01a17ce2-cf8d-7b60-adef-489366915c98`.
This describes technical contracts; UI wording belongs to the UI owner.
The new service PATCH must be integrated with the OpenAPI delta supplied to the
parent before release. It is not a claim that this route is already deployed.

## Service credentials

`PATCH /internal/v1/service-tokens/{service_id}` requires a **global** service
credential with exact scope `service_tokens:write`. Tenant-scoped writers are
forbidden, including when editing themselves. Status changes retain their
existing `PATCH /internal/v1/service-tokens/{service_id}/status` contract.

```ts
type UUID = string;
type ServiceScope =
  | 'credits:read' | 'credits:write'
  | 'entitlements:read' | 'entitlements:write'
  | 'filter_assistant:execute'
  | 'generations:quarantine:read' | 'generations:reconcile' | 'generations:write'
  | 'keys:read' | 'keys:write' | 'metrics:read' | 'oauth:write'
  | 'plugins:read' | 'plugins:write' | 'prices:read' | 'prices:write'
  | 'providers:read' | 'providers:write' | 'requests:read'
  | 'settlements:adjust' | 'routes:read' | 'routes:write' | 'schemas:read'
  | 'service_tokens:read' | 'service_tokens:write'
  | 'tenants:read' | 'tenants:write' | 'upstreams:import:write';

interface UpdateServiceCredential {
  name?: string;
  scopes?: ServiceScope[];
  tenant_external_id?: string | null;
}

interface ServiceCredential {
  service_id: UUID;
  name: string;
  status: 'active' | 'suspended' | 'revoked';
  credential_generation: number;
  fingerprint: string;
  credential_copy_available: boolean;
  scopes: ServiceScope[];
  tenant_external_id: string | null;
  created_at: number;
  updated_at: number;
}
```

- Submit at least one field. Omitted fields retain their stored values.
- `tenant_external_id: null` explicitly selects global scope; omission retains
  the tenant. This is a privilege-sensitive change available only to the global
  writer. Never infer null from an empty tenant picker or failed tenant load.
- Names and tenant IDs are trimmed; nonblank names are limited to 120 UTF-8
  bytes and tenant IDs to 200 bytes; control characters are rejected.
- Scopes replace the complete set. At least one unique supported exact scope is
  required. `*`, `analytics:read`, unknown values, duplicates and empty arrays are
  rejected. Null `name`/`scopes` and unknown properties fail the same way:
  the control plane normalizes every request-shape rejection to a uniform 400.
- 200 returns `ServiceCredential`, with no token, key or other secret.
  Generation, fingerprint and the existing secret remain unchanged.
- Validation and malformed request shapes return 400; missing/invalid
  authentication 401; insufficient scope
  or a tenant-scoped caller 403; missing identity/current generation 404;
  revoked identity 409. Suspended credentials can be edited and remain suspended.
- A principal lock serializes updates with rotation. Credential fields are read
  in a second statement after acquiring that lock, so a waiting name-only edit
  does not restore older scopes. SQLite uses its existing immediate write transaction.
- New authentication uses the updated database scopes and tenant immediately.
  Already-authenticated in-flight requests retain their existing receipt.
  Rotation receipts remain historical issuance receipts: replaying an earlier
  rotation may describe older metadata, but does not restore authority. Reload
  the service list for current metadata rather than treating a replay as current.

## Existing client credential APIs

Client keys do not have service scopes. Their creation fields are not a single
mutable document: identity, funds, policy and routing have different authorities.

| Data | Existing operation | Authorization / behavior |
| --- | --- | --- |
| Alias | `PATCH /internal/v1/keys/{key_id}/alias`, `{alias: string}` | `keys:write`; tenant checked; response `{key_id, alias, updated_at}` |
| Status | `PATCH /internal/v1/keys/{key_id}/status`, `{status}` | `keys:write`; tenant checked; revoked is terminal |
| Rate limits / enforcement / budgets | `PUT /internal/v1/keys/{key_id}/policy` | `keys:write`; **complete replacement**, active keys only |
| Limit/usage snapshot | `GET /internal/v1/keys/{key_id}/limits` | `keys:read`; tenant checked |
| Models | `GET /internal/v1/keys/{key_id}/routing?tenant_external_id=...` | `keys:read`; normalized route / route-group grants |
| Model grants | `PUT /internal/v1/keys/{key_id}/routing` | `keys:write`; body below; stale grant revision returns 409 |
| Credit changes | `POST /internal/v1/accounts/{account_id}/grants` | `credits:write`; financial grant with idempotency, not metadata edit |
| Tenant / principal / currency / creation source | Creation only | No existing generic client PATCH or rebind API. This change does not reassign financial identities. |

```ts
interface ClientPolicy {
  requests_per_minute?: number | null;
  tokens_per_minute?: number | null;
  max_concurrency?: number | null;
  enforcement_mode?: 'prepaid' | 'metered_unlimited';
  daily_budget?: string | null;
  weekly_budget?: string | null;
  lifetime_budget?: string | null;
}
interface ReplaceClientRouting {
  tenant_external_id: string;
  route_ids: UUID[];
  route_group_ids: UUID[];
  expected_grant_revision: number;
}
```

On this master base, omitted/null rate limits mean no configured limit. Explicit
limits must be positive and TPM must be JSON-safe. Policy PUT is not a partial
PATCH: populate the complete current editor state and preserve unrelated budgets
and enforcement mode. `allowed_models` is not accepted in public policy input.
Budgets use decimal strings in the account currency. Changing a budget does not
grant credit or reset settled/reserved usage. Initial balance is issuance input,
not an editable current balance. Model grants belong in the routing endpoint.

## Analysis and search: independent authorization

| Surface | Minimum credential / scope | Boundary |
| --- | --- | --- |
| `/internal/v1/usage-analysis`, `/trends` | Service + `requests:read` | Token tenant is enforced; only global tokens can select global scope |
| `/internal/v1/requests`, `/requests/query`, request details/events | Service + `requests:read` | Same management tenant boundary |
| `/internal/v1/sessions` | Service + `requests:read` | Requires resolved tenant; scoped token supplies its own tenant |
| `/internal/v1/tenants` | Service + `requests:read` | Scoped token sees only its own tenant; no tenants:read needed for this discovery API |
| `/self/v1/usage-analysis` | Active client key | Exact authenticated `(tenant_id,key_id)` only; no service scope |
| `/self/v1/requests` | Active client key | Own key only; not a management credential |

`metrics:read` is for operational metrics/version, not management analysis.
There is no `analytics:read` scope. A client secret cannot authenticate to a
management API; a service credential cannot authenticate to the self API.
Self analysis accepts only `from_created_at`, `to_created_at`, `granularity`,
`model`, `protocol`, `status`, `error_code`; tenant/key/principal/upstream/route
selectors are rejected. It returns `Cache-Control: private, no-store` and excludes
management identity dimensions. Self request query has a different legacy shape;
do not assume it rejects all management-looking fields as self analysis does.

Analysis/search pages may call auxiliary APIs with additional permissions:

- Client lookup `keys:read`; editing `keys:write`.
- Upstream names/provider types/groups require `providers:read`.
- Route names and `/internal/v1/filter-model-names` require `routes:read`.
- `/internal/v1/model-picker-options` requires both routes:read and providers:read.
- `/internal/v1/schemas` requires `schemas:read`; `/internal/v1/plugins` plugins:read.
- Assistant execution requires `filter_assistant:execute`, separately from search.

These optional 403s must not be treated as failure of the analysis/search
credential. Allow typed request/model IDs and direct search where catalogs are
unavailable. Preserve core results and distinguish unavailable optional metadata.
No scopes should be auto-added to make optional catalogs load.

## CI handoff

Nine unit/HTTP regression cases live in
`src/api/credentials/update_tests.rs`. Run in GHA, including the focused filter
`cargo test --locked --all-features --lib api::credentials::update_tests` and
the repository OpenAPI contract job after integrating the supplied OpenAPI delta.
Set `MTC_TEST_POSTGRES_URL` for the PostgreSQL lock-wait regression; it is skipped
when the variable is absent. That case holds a principal lock, observes the
waiting metadata update, changes scopes in the first transaction, and checks that
the waiting name-only update preserves the committed scope and tenant.
No local build, test or dependency install was run. Parent must publish the
combined candidate to trigger GHA; this worker has not committed, pushed or
changed any production permission.
