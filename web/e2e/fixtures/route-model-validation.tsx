import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { UpstreamModelCombobox } from '../../src/operator/UpstreamModelCombobox';
import type { UpstreamAccount } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const params = new URLSearchParams(location.search);
const scenario = params.get('scenario') ?? 'stale';
const accounts = [
  { id: 'a', name: 'Catalog account', driver: 'openai', connection_method: 'api_key' },
  { id: 'b', name: 'Codex OAuth', driver: 'openai-codex', connection_method: 'oauth' },
] as UpstreamAccount[];

function Fixture() {
  const [model, setModel] = useState(params.get('model') ?? 'gpt-5.6-luna');
  const [accountIds, setAccountIds] = useState(['a', 'b']);
  const [protocol, setProtocol] = useState('openai');
  const [groupIds, setGroupIds] = useState(scenario === 'group' || scenario === 'members' ? ['group'] : []);
  const [memberIds, setMemberIds] = useState(['a', 'b']);
  const [validity, setValidity] = useState({ valid: false, custom: false });
  const [submitted, setSubmitted] = useState('');
  return <main style={{ padding: 24, maxWidth: 760 }}>
    <h1>New model route</h1>
    <UpstreamModelCombobox token="fixture" tenant="tenant" upstreams={accounts}
      accountIds={accountIds} includedProviderGroupIds={groupIds}
      excludedProviderGroupIds={[]} syncAccountIds={groupIds.length ? memberIds : accountIds} protocol={protocol}
      value={model} onChange={setModel} customModelConfirmed={params.has('editing')}
      onValidityChange={(valid, custom) => setValidity({ valid, custom })} />
    <button disabled={!validity.valid} onClick={() => setSubmitted(JSON.stringify({
      upstream_model: model, upstream_account_ids: accountIds, custom_model_confirmed: validity.custom,
    }))}>Save route</button>
    <button onClick={() => setModel('gpt-5.6-terra')}>Change model</button>
    <button onClick={() => setAccountIds(['b'])}>Change candidates</button>
    <button onClick={() => setProtocol('anthropic')}>Change protocol</button>
    <button onClick={() => setGroupIds((current) => current.length ? [] : ['group'])}>Toggle group</button>
    <button onClick={() => setMemberIds(['b'])}>Change members</button>
    <output data-submitted>{submitted}</output>
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
