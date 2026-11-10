import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { UpstreamQuotaResetSection } from '../../src/operator/UpstreamQuotaPanel';
import { PluginUpstreamAccountResetSettings, registerOperatorPluginContributions } from '../../src/operator/pluginContributions';
import type { PluginManifest } from '../../src/types';

const manifest: PluginManifest = { id: 'reset-extension', version: '1.0.0', wit_version: '0.2.0', capabilities: [], contributions: { operator_ui: [{
  id: 'settings', slot: 'operator.upstream-account.reset-settings', label: 'Automatic settings', icon: 'plug', renderer: 'component_v1',
  module_entry: 'ui/settings.mjs', module_sha256: `sha256:${'a'.repeat(64)}`, component_id: 'settings',
}] } };
function Fixture() {
  const [account, setAccount] = useState('account-one');
  const [tenant, setTenant] = useState('tenant-one');
  const [installed, setInstalled] = useState(true);
  const registry = registerOperatorPluginContributions(installed ? [manifest] : []);
  return <main style={{ padding: 16, maxWidth: 720 }}>
    <button onClick={() => setAccount('account-two')}>Change account</button>
    <button onClick={() => setTenant('tenant-two')}>Change tenant</button>
    <button onClick={() => setInstalled(false)}>Remove extension</button>
    <output data-testid="navigation-count">{registry.navigation.length}</output>
    <UpstreamQuotaResetSection accountId={account} accountName="Current account" tenant={tenant} token="fixture-only"
      resetSettings={<PluginUpstreamAccountResetSettings contributions={registry.upstreamAccountResetSettings} token="fixture-only"
        accountId={account} accountName="Current account" provider="openai-codex" tenant={tenant} locale="en" onNavigate={() => { throw new Error('Unexpected navigation'); }} />} />
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
