# Token Center Operator UI SDK

This package defines the versioned React contract for MTC Operator extensions. A plugin UI package exports one `operatorUiPackage` value created with `defineOperatorUiPackage`.

```tsx
import { defineOperatorUiPackage } from '@memeloop/token-center-operator-ui-sdk';
import { Workspace } from './Workspace.js';

export const operatorUiPackage = defineOperatorUiPackage({
  apiVersion: 'operator-ui-package-v1',
  pluginId: 'example-plugin',
  compatiblePluginVersions: ['1.0.0'],
  components: { workspace: Workspace },
});
```

The component receives tenant and locale context plus helpers for MTC API requests, service-data feeds, and Operator navigation. See the product documentation under `docs/en/plugins/operator-ui.md` and `docs/zh/plugins/operator-ui.md` for manifest examples and build integration.
