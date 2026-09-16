# Operator UI extensions

A plugin can contribute a sidebar tab or overview card to the Operator console with **no browser code**: the manifest declares the data source and fixed slot, and the core trusted `typed_data_v1` renderer turns JSON into a native component. The contract is defined by `contributions.service_data` and `contributions.operator_ui` in the [manifest schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json), together with the [UI projection schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-ui-projection.schema.json).

## Manifest example

```json
{
  "capabilities": [
    { "kind": "http", "allowed_origins": ["https://plugin-api.example.com"] }
  ],
  "contributions": {
    "service_data": [
      {
        "id": "health",
        "url": "https://plugin-api.example.com/v1/health",
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
      }
    ],
    "operator_ui": [
      {
        "id": "health-tab",
        "slot": "operator.sidebar.tab",
        "category": { "id": "monitoring" },
        "route": "plugin-health",
        "label": "Plugin health",
        "icon": "heart",
        "renderer": "typed_data_v1",
        "data_endpoint": "health"
      }
    ]
  }
}
```

Key points:

- `service_data.url` must be a complete HTTPS GET URL, and its origin must exactly match the `allowed_origins` of an `http` capability. `fallback` must validate against `response_schema` so the UI remains usable when the upstream fails.
- Choose one slot: `operator.sidebar.tab` (a sidebar tab, requiring `route` and a category—`monitoring`, `traffic`, `identity`, and `system` append to core categories; a new category needs a bounded `category.label`) or `operator.overview.card` (an overview card).
- `icon` can only be `activity`, `chart`, `database`, `heart`, `plug`, or `shield`; `data_endpoint` must point to a `service_data` ID from the same plugin.
- `presentation` can be omitted (generic key-value view), set to `"health_intelligence_v1"` (compact core three-source health view), or `"projection_v1"` (structured projection, below).
- Loading rejects core route names, duplicate routes, category conflicts, arbitrary icon names, and unknown data endpoints immediately; `settings` and `system-settings` are reserved by the core.

## Data flow

The browser never accesses a plugin URL directly; it calls a core proxy endpoint:

![The browser reads plugin data from the MTC control plane; the control plane checks permission and tenant, makes a restricted GET to a fixed plugin endpoint, validates JSON, and returns a core-owned envelope.](/diagrams/plugin-ui.svg)

MTC never forwards service credentials, browser tokens, or tenant values to plugin endpoints. The response is always a core-owned envelope:

```json
{
  "data": { "status": "healthy" },
  "partial": false,
  "provenance": {
    "plugin_id": "example-observability",
    "endpoint_id": "health",
    "origin": "https://plugin-api.example.com",
    "fetched_at": 1780000000000,
    "source": "network"
  }
}
```

When the upstream fails, `partial: true` and `source` is `stale_cache` or `fallback`; `data` remains schema-valid. Provide a meaningful, non-sensitive fallback.

## `projection_v1` projection

With `"presentation": "projection_v1"`, endpoint data must use the projection format, with `slot_id` set to the contribution's `id`:

```json
{
  "schema_version": 1,
  "plugin_id": "example-observability",
  "slot_id": "health-tab",
  "components": [
    { "kind": "metric", "label": "Request count", "value": "42" },
    { "kind": "status", "label": "Status", "state": "ok" },
    { "kind": "text", "text": "Data from the example plugin" },
    { "kind": "link", "label": "Details", "href": "https://plugin-api.example.com/dashboard" }
  ]
}
```

Component fields depend on `kind`: `text` uses `text`; `metric` uses `label` + `value`; `status` uses `label` + `state` (`ok`, `warning`, `error`, `unknown`); `link` uses `label` + `href` (HTTPS only). Each slot accepts at most 32 components. A link's origin must come from an `http` capability approved by the installed manifest; projection data cannot claim its own origin.

## Security boundaries

`typed_data_v1` has no iframe, remote HTML, remote stylesheet, arbitrary JavaScript, or credential forwarding: React uses core components to render JSON as plain text. Installation and removal only change the loaded manifest set. After removal, that plugin's tabs, cards, and `plugin--{plugin_id}--{route}` routes disappear at the next manifest refresh, with no navigation records left behind.
