# Operator UI extensions

Plugins can add sidebar tabs and overview cards to the Operator console. MTC supports two integration paths:

- `typed_data_v1` renders structured data with the MTC design system.
- `component_v1` connects a trusted React package through the versioned Operator UI SDK for interactive workspaces, visualizations, and complete workflows.

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
        "component_id": "health-workspace",
        "component_props": { "defaultRange": "24h" },
        "data_endpoint": "health"
      }
    ]
  }
}
```

`operator.sidebar.tab` uses a `route` and `category`; `operator.overview.card` appears on the overview; `operator.page.before` and `operator.page.after` use `target_route` to mount a component inside an existing Operator page. Core categories are `monitoring`, `traffic`, `identity`, and `system`. A plugin may also declare a named category. Available icons are `activity`, `chart`, `database`, `heart`, `plug`, and `shield`.

## React package

The package exports `operatorUiPackage`:

```tsx
import { defineOperatorUiPackage } from '@memeloop/token-center-operator-ui-sdk';
import { HealthWorkspace } from './HealthWorkspace';

export const operatorUiPackage = defineOperatorUiPackage({
  apiVersion: 'operator-ui-package-v1',
  pluginId: 'example-observability',
  compatiblePluginVersions: ['1.0.0'],
  components: { 'health-workspace': HealthWorkspace },
});
```

Each component receives plugin, tenant, locale, slot, and host API context. The host API exposes:

- `loadServiceData(endpointId)` for manifest service-data feeds.
- `request(path, options)` for MTC APIs available to the current Operator credential.
- `navigate(route)` for core and installed-plugin routes.

Components can use React, Fluent UI, charting packages, and their own state management. MTC theme variables and the Fluent Provider cover the mounted component tree.

Register packages at build time with a comma-separated `MTC_OPERATOR_UI_PACKAGES` value:

```bash
MTC_OPERATOR_UI_PACKAGES=@memeloop/health-plugin-ui,@memeloop/routing-plugin-ui npm run build
```

The installed manifest activates a contribution and the compiled package supplies its component. Manifest removal and version changes update the corresponding tabs, cards, and routes on the next catalog refresh.

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

Build-time module resolution keeps component packages inside the standard TypeScript, dependency-locking, review, and content-security-policy pipeline. Runtime manifests select compiled components with a stable, traceable loading path.
