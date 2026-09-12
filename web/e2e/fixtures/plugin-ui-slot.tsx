import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { PluginUiSlot } from '../../src/plugins/PluginUiSlot';
import '../../src/styles.css';
import '../../src/theme.css';

const messages = { loading: 'Loading', unavailable: 'Plugin unavailable', empty: 'No data', states: { ok: 'Healthy', warning: 'Warning', error: 'Error', unknown: 'Unknown' } };
function Fixture() {
  const [scope, setScope] = useState('alpha');
  return <main style={{ padding: 16, minWidth: 0 }}>
    <button onClick={() => setScope('beta')}>Switch tenant</button>
    <button onClick={() => { document.documentElement.dataset.theme = 'light'; }}>Light theme</button>
    <div style={{ height: '150vh' }} aria-hidden="true" />
    {['healthy', 'broken'].map((pluginId) => <PluginUiSlot key={pluginId}
      pluginId={pluginId} slotId="summary" title={pluginId} scopeKey={scope}
      allowedLinkOrigins={['https://example.com']} messages={messages}
      load={(signal) => fetch(`/fixture/projection/${pluginId}?tenant=${scope}`, { signal }).then((response) => response.json())} />)}
  </main>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
