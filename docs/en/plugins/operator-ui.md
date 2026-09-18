# Operator UI extensions

Plugins can add sidebar tabs and overview cards to the Operator console. MTC supports two integration paths:

- `typed_data_v1` renders structured data with the MTC design system.
- `component_v1` loads a signed, digest-addressed React module from the installed plugin package for interactive workspaces, visualizations, and complete workflows.

The manifest contract lives in [plugin-manifest.schema.json](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json). The publishable React contract lives in `web/operator-ui-sdk`.

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
          "required": ["status"],
          "properties": { "status": { "type": "string" } }
        },
        "fallback": { "status": "offline" }
      }
    ],
    "operator_ui": [
      {
        "id": "health-tab",
        "slot": "operator.sidebar.tab",
        "category": { "id": "monitoring" },
        "route": "plugin-health",
        "label": "Service health",
        "icon": "heart",
        "renderer": "component_v1",
        "module_entry": "assets/operator-ui.mjs",
        "component_id": "health-workspace",
        "component_props": { "defaultRange": "24h" },
        "data_endpoint": "health"
      }
    ]
  }
}
```

`operator.sidebar.tab` uses a `route` and `category`; `operator.overview.card` appears on the overview; `operator.page.before` and `operator.page.after` use `target_route` to mount a component inside an existing Operator page. Core categories are `monitoring`, `traffic`, `identity`, and `system`. A plugin may also declare a named category. Available icons are `activity`, `chart`, `database`, `heart`, `plug`, and `shield`.

## React module

The signed OCI artifact includes a self-contained ESM asset. Its activation export receives MTC's React and Fluent runtimes, preserving one React instance across the host and every plugin:

```js
export function activateOperatorUi({ React, Fluent, defineOperatorUiPackage }) {
  function HealthWorkspace({ api, contribution }) {
    return React.createElement(Fluent.Text, null, contribution.label);
  }
  return defineOperatorUiPackage({
    apiVersion: 'operator-ui-package-v1',
    pluginId: 'example-observability',
    compatiblePluginVersions: ['1.0.0'],
    components: { 'health-workspace': HealthWorkspace },
  });
}
```

Each component receives plugin, tenant, locale, slot, and host API context. The host API exposes:

- `loadServiceData(endpointId)` for manifest service-data feeds.
- `navigate(route)` for core and installed-plugin routes.

Components use the host's React and Fluent UI objects and can bundle other browser-only libraries into the single ESM asset. MTC theme variables and the Fluent Provider cover the mounted component tree.

Publish the module as a plugin asset layer alongside `plugin.json`:

```bash
oras push --artifact-type application/vnd.memeloop.token-center.plugin.v1 \
  --config artifact-config.json:application/vnd.memeloop.token-center.plugin.config.v1+json \
  ghcr.io/example/example-observability:1.0.0 \
  plugin.json:application/vnd.memeloop.token-center.plugin.manifest.v1+json \
  assets/operator-ui.mjs:application/vnd.memeloop.token-center.plugin.asset.v1
```

Installation verifies the pinned OCI digest and signature. The active runtime captures the exact module bytes, publishes their SHA-256 in the plugin catalog, and serves them from an immutable same-origin URL. Publishing, rolling back, or uninstalling a plugin updates its tabs and page slots on the next catalog refresh without rebuilding MTC. Each contribution has its own loading state and error boundary.

## Structured-data path

Compact surfaces can use `typed_data_v1`. `data_endpoint` names a `service_data` entry from the same manifest. `presentation` selects the generic key-value view, `health_intelligence_v1`, or `projection_v1`. The projection format supports text, metric, status, and link components; see the [UI projection schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-ui-projection.schema.json).

The control plane reads and validates service data and returns a consistent envelope:

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

`component_v1` modules stay inside the signed plugin artifact and MTC content-security policy. The first contract uses one ESM file per entry; a future SDK version can add chunk manifests while keeping the content-addressed loading model.
