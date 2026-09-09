import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { api } from '../../src/api';
import { I18nProvider } from '../../src/i18n';
import { Plugins } from '../../src/operator/Plugins';
import { useOperatorResource } from '../../src/operator/hooks/useOperatorResource';
import type { PluginManifest } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const manifests: PluginManifest[] = ['first', 'second', 'third'].map((id) => ({
  id, version: '1.0.0', wit_version: '1', capabilities: [],
  contributions: { configuration: { default: {}, schema: { type: 'object', properties: { mode: { type: 'string', title: 'Mode' } } } } },
}));

function Fixture() {
  const [tenant, setTenant] = useState('alpha');
  const [show, setShow] = useState(true);
  const resource = useOperatorResource(true, tenant, (signal) => api<{ tenant: string }>(`/fixture/resource?tenant=${tenant}`, 'fixture-service', { signal }), 'Failed');
  return <main>
    <button onClick={() => setTenant('beta')}>Switch tenant</button>
    <button onClick={() => setShow((value) => !value)}>Toggle plugins</button>
    <output>{resource.state.kind === 'ready' ? resource.state.value.tenant : resource.state.kind}</output>
    {show && <Plugins token="fixture-service" tenant={tenant} values={manifests} />}
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
