# Token Center Operator UI SDK

This package defines the versioned React contract for MTC Operator extensions. The signed plugin artifact carries one self-contained ESM entry. MTC loads that exact module after installation and passes its own React and Fluent runtimes to `activateOperatorUi`, so installing or upgrading a plugin does not rebuild MTC.

Hosts also provide optional `ReactDOM` and `ReactJSXRuntime` bindings containing the complete `react-dom` and `react/jsx-runtime` namespaces. Packages that use portals or automatic JSX should check these bindings during activation and map their bundled externals to the host bindings before evaluating component libraries. This keeps hooks, contexts and portals on the host's React instance. Existing packages that only read `React` and `Fluent` keep the same activation contract.

```tsx
export function activateOperatorUi({ React, Fluent, defineOperatorUiPackage }) {
  function Workspace({ contribution }) {
    return React.createElement(Fluent.Text, null, contribution.label);
  }
  return defineOperatorUiPackage({
    apiVersion: 'operator-ui-package-v1',
    pluginId: 'example-plugin',
    compatiblePluginVersions: ['1.0.0'],
    components: { workspace: Workspace },
  });
}
```

The component receives tenant and locale context, declared service-data feeds, and the host API below. Bundle the entry as one ESM file and publish it as a signed plugin asset layer. See the product documentation under `docs/en/plugins/operator-ui.md` and `docs/zh/plugins/operator-ui.md`.

- `api.requestApi({ path, method, body, signal })` sends JSON to a same-origin product API using the current Operator credential. The plugin cannot set credentials, headers, redirects, or another origin. Backend scopes and resource authorization still apply. The bridge supplies the selected tenant in query parameters and in tenant-aware write bodies discovered from OpenAPI; undocumented writes are rejected.
- `api.discoverApi(signal)` returns the authenticated OpenAPI document from `/internal/v1/api-schema`. Cache or filter relevant operations before sending them to a model.
- `api.navigate(route, context?)` navigates through the host's unsaved-change guard. The destination receives optional in-memory `props.context`; this context expires on navigation away or a tenant/credential change.

`operator.model-route.action` is a component-only slot rendered in each model-route row. The component receives `context.modelRoute`. It has no sidebar category, route, or target route; a separate `operator.sidebar.tab` contribution owns the destination page. Neither the host nor this SDK defines a playground feature.

For temporary inference, `POST /internal/v1/inference/chat/completions` and `POST /internal/v1/inference/messages` accept `{ key_id, request }`, where `request` is a non-streaming OpenAI chat or Anthropic Messages request respectively. The signed-in service needs `keys:write` for that existing, tenant-authorized billing key. The host retains the plaintext credential and applies the key's normal model policy, balance, routing and usage accounting. A service credential alone does not identify an inference principal. Temporary UI chat history is a plugin concern; normal gateway request records and billing still apply.
