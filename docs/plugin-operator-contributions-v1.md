# Operator plugin contributions v1

This contract lets a signed Token Center plugin contribute an operator sidebar
tab or an overview card without shipping browser code. It is intended for
official service-plugin repositories such as CodexRadar, DeepSWE, and CDK;
those repositories remain independently versioned and are not vendored here.

The authoritative machine-readable contract is
[`schemas/plugin-manifest.schema.json`](../schemas/plugin-manifest.schema.json)
and the runtime endpoint is documented in
[`openapi/openapi.yaml`](../openapi/openapi.yaml).

## Manifest surface

Add `contributions.service_data` and `contributions.operator_ui` to `plugin.json`.
`service_data` names a complete HTTPS GET URL, an existing read scope required
to access it, an object-root response JSON Schema, a schema-valid fallback, and
bounded cache/timeout/body limits. Its origin must exactly match an existing
`capabilities: [{ "kind": "http", "allowed_origins": [...] }]` entry.

`operator_ui` selects exactly one of these fixed slots:

- `operator.sidebar.tab` requires a non-core route token and a category. Use
  `monitoring`, `traffic`, `identity`, or `system` to append to a core category;
  a new category requires a bounded `category.label`.
- `operator.overview.card` has no category or route and appears in the fixed
  operator overview card region.

Every contribution must use `renderer: "typed_data_v1"`, one of the documented
icon tokens (`activity`, `chart`, `database`, `heart`, `plug`, `shield`), and a
same-plugin `data_endpoint`. `presentation` is optional and closed: omitting
it selects the generic typed-data view, while `health_intelligence_v1` selects
the core's compact three-source health view after its exact bounded snapshot is
validated. It never names plugin JavaScript or a browser URL. Core routes,
duplicate routes, conflicting new category labels, write-only response fields,
arbitrary icon names, and unknown data endpoints are rejected at load time.
`settings` and `system-settings` are also reserved for core system
configuration, whether or not that screen is enabled by a given Token Center
build.

Example fragment:

```json
{
  "capabilities": [{ "kind": "http", "allowed_origins": ["https://radar.example"] }],
  "contributions": {
    "service_data": [{
      "id": "health",
      "url": "https://radar.example/v1/health",
      "required_scope": "metrics:read",
      "response_schema": {
        "type": "object",
        "additionalProperties": false,
        "required": ["status"],
        "properties": { "status": { "type": "string" } }
      },
      "fallback": { "status": "offline" },
      "cache_ttl_seconds": 30,
      "timeout_millis": 2000,
      "max_body_bytes": 65536
    }],
    "operator_ui": [{
      "id": "health-and-intelligence",
      "slot": "operator.sidebar.tab",
      "category": { "id": "monitoring" },
      "route": "health-intelligence",
      "label": "Health and intelligence",
      "icon": "heart",
      "renderer": "typed_data_v1",
      "presentation": "health_intelligence_v1",
      "data_endpoint": "health"
    }]
  }
}
```

## Service data wire contract

The browser never calls a plugin URL. It calls:

```
GET /internal/v1/plugins/{plugin_id}/data/{endpoint_id}?tenant_external_id=...
```

The caller needs `plugins:read` plus the endpoint's `required_scope`; tenant
credentials remain bounded by the standard control-plane tenant authorization.
Token Center sends no incoming service credential, browser token, tenant value,
or caller-controlled headers to the plugin URL. It performs only a GET, rejects
non-HTTPS/non-allowlisted destinations, pins public DNS through the existing
SSRF boundary, disables redirects, applies the declared timeout/body cap, and
accepts only JSON that validates against `response_schema`.

The response is always core-owned typed JSON:

```json
{
  "data": { "status": "healthy" },
  "partial": false,
  "provenance": {
    "plugin_id": "observability-suite",
    "endpoint_id": "health",
    "origin": "https://radar.example",
    "fetched_at": 1760000000000,
    "source": "network"
  }
}
```

When upstream retrieval fails, `partial` is `true` and `source` is
`stale_cache` or `fallback`; the `data` shape remains schema-valid. Plugins
must therefore provide a useful, non-secret fallback.

## Explicit non-goals and release compatibility

`typed_data_v1` has no iframe, remote HTML, remote stylesheet, arbitrary
JavaScript, module federation, arbitrary navigation, or credential-passthrough
surface. React renders JSON as text using components owned by Token Center.
Install/uninstall changes only the loaded manifest set; it does not patch core
code or persist a navigation record. On the next manifest refresh all of that
plugin's tabs, cards, and opaque `plugin--{plugin_id}--{route}` routes vanish.

External official example repositories should publish a signed OCI package with
this manifest shape, target Token Center's published OpenAPI and JSON Schema,
and treat the allowed icon/slot/renderer sets as closed v1 enums. They should
not depend on private Token Center React modules, database tables, browser
credential storage, or a URL beyond the documented core proxy route.
