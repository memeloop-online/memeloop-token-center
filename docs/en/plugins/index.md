# Plugin overview

MTC plugins are **versioned WebAssembly components** (Component Model), not dynamically loaded native libraries. Each plugin package contains a `plugin.json` manifest, an optional `.wasm` component, a README, and icons. The manifest declares contributions and capabilities, and the host executes the component within explicit boundaries.

Authoritative contracts:

- [WIT interface definition `token-center.wit`](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit)
- [`plugin.json` manifest schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json)
- Management endpoints are documented in [OpenAPI](https://github.com/memeloop-online/memeloop-token-center/blob/master/openapi/openapi.yaml) under `/internal/v1/plugins*` and `/internal/v1/plugin-runtime*`

## Capability model

What a plugin can do is determined entirely by its manifest `capabilities`:

| Capability | Meaning |
| --- | --- |
| `log` | Emit bounded host log events (without plugin-provided raw text) |
| `kv` | Key-value storage isolated in the plugin namespace |
| `http` | Access only origins listed exactly in the manifest; redirects are forbidden and requests/responses are bounded |
| `group_routing_quota` | Let a group-routing plugin read quota-window snapshots for authorized candidates (see [Group routing](routing.md)) |

The host provides bounded fuel, memory, and execution time for every call. The core system always retains credential injection, destination validation, authorization, pricing, quotas, accounting, archiving, and error sanitization. Components never see upstream credential material and cannot bypass model permissions, balances, limits, or audit.

## Installation and publishing

Plugin packages are distributed as signed OCI artifacts (see [Plugin development](development.md) for packaging). Operators complete the full workflow on the **Plugins** page of the Operator console; installation and activation always require the global `plugins:write` scope:

![Plugin flow from installation verification, manifest review, and artifact approval to publishing; rollback restores prior content through a new revision.](/diagrams/plugin-publication.svg)

- Installation only pulls, verifies, and stages the artifact; it **does not activate code**. Approval moves it into the candidate inventory, and publishing makes it globally effective.
- Publishing and rollback both use revision CAS plus an idempotency key. A rollback always creates a new monotonically increasing revision; it never moves the counter backward.
- In-flight requests use the plugin snapshot captured when they entered; a new publication does not affect them.

Management API (all require global scope; read uses `plugins:read`, write uses `plugins:write`):

| Method and path | Purpose |
| --- | --- |
| `GET /internal/v1/plugin-runtime` (or `/candidates`) | Current revision and available/staged version IDs |
| `GET /internal/v1/plugin-runtime/history` | Installation jobs, version history, and operator audit |
| `POST /internal/v1/plugin-runtime/installations` | Start an installation job (`Idempotency-Key`, 202) |
| `GET /internal/v1/plugin-runtime/installations/{id}` | View the job and manifest-review details |
| `POST /internal/v1/plugin-runtime/installations/{id}/approve` | Approve registration from the review summary |
| `POST /internal/v1/plugin-runtime/installations/{id}/retry` | Retry an interrupted or failed job |
| `POST /internal/v1/plugin-runtime/publish` | Publish `{inventory_id, expected_revision}` |
| `POST /internal/v1/plugin-runtime/rollback` | Roll back `{target_revision, expected_revision}` |
| `GET /internal/v1/plugins`, `GET /internal/v1/plugins/runtime-access` | Loaded manifests and the current credential's plugin permissions |

## Plugin configuration

A plugin can declare a JSON Schema for its object root and non-sensitive defaults in the manifest. Operators maintain configuration through a form or API:

```bash
curl "https://mtc.example.com/internal/v1/plugins/example-policy/configuration" \
  -H "Authorization: Bearer mts_example_service_token"
```

- Precedence is **tenant override > global value > manifest default**.
- `PUT` requires `plugins:write`, `Idempotency-Key`, and the current `expected_version`; conflicts return 409 and can be safely replayed.
- The configuration schema forbids `writeOnly` fields. Secrets such as API keys and OAuth tokens cannot be put in plugin configuration; use core encrypted credential storage.

## Extension points

| Extension point | Documentation |
| --- | --- |
| Traffic policy / request rewrite (`traffic-policy.post-auth`) | [Plugin development](development.md) |
| Upstream Provider and OAuth (`upstream-provider`) | [Plugin development](development.md) |
| Group-routing plan/observe (`group-routing-v1`) | [Group routing](routing.md) |
| Operator sidebar tab / overview card (`typed_data_v1`) | [Operator UI](operator-ui.md) |
