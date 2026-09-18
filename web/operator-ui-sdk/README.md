# Token Center Operator UI SDK

This package defines the versioned React contract for MTC Operator extensions. The signed plugin artifact carries one self-contained ESM entry. MTC loads that exact module after installation and passes its own React and Fluent runtimes to `activateOperatorUi`, so installing or upgrading a plugin does not rebuild MTC.

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

The component receives tenant and locale context plus its declared service-data feeds and Operator navigation. Bundle the entry as one ESM file and publish it as a signed plugin asset layer. See the product documentation under `docs/en/plugins/operator-ui.md` and `docs/zh/plugins/operator-ui.md`.
