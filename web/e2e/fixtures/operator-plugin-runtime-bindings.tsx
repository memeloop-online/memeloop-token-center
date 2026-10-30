import * as React from 'react';
import * as ReactDOM from 'react-dom';
import * as ReactJSXRuntime from 'react/jsx-runtime';
import { createRoot } from 'react-dom/client';
import { OperatorPluginComponentHost } from '../../src/plugins/OperatorPluginComponentHost';

Object.defineProperty(window, '__operatorRuntimeBindingsFixture', {
  value: { React, ReactDOM, ReactJSXRuntime },
});

createRoot(document.getElementById('root')!).render(<OperatorPluginComponentHost
  pluginId="runtime-bindings"
  pluginVersion="1.0.0"
  contribution={{
    id: 'workspace', slot: 'operator.sidebar.tab', route: 'workspace', label: 'Runtime fixture', icon: 'plug',
    renderer: 'component_v1', module_entry: 'assets/operator-ui.mjs',
    module_sha256: `sha256:${'b'.repeat(64)}`, component_id: 'workspace',
  }}
  serviceEndpointIds={[]}
  credential="mts_runtime_fixture"
  tenantExternalId="alpha"
  locale="en"
  onNavigate={() => {}}
/>);
