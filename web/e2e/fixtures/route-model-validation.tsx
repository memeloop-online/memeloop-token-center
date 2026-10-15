import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { UpstreamModelCombobox } from '../../src/operator/UpstreamModelCombobox';
import type { UpstreamAccount } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

function Fixture() {
  const parameters = new URLSearchParams(location.search);
  const [value, setValue] = useState(parameters.get('model') ?? 'gpt-5.6-luna');
  const [account, setAccount] = useState('first');
  const [token, setToken] = useState('fixture');
  const [tenant, setTenant] = useState('fixture');
  const [group, setGroup] = useState('group-first');
  const [excluded, setExcluded] = useState<string[]>([]);
  const [protocol, setProtocol] = useState(parameters.get('protocol') || 'openai');
  const [valid, setValid] = useState(false);
  const [custom, setCustom] = useState(false);
  const grouped = parameters.has('group');
  const upstreams = [account].map(id => ({ id, name: 'Selected account', driver: parameters.get('driver') || 'custom', connection_method: 'oauth' })) as UpstreamAccount[];
  return <main style={{ padding: 12 }}>
    <button onClick={() => setAccount(current => current === 'first' ? 'second' : 'first')}>Change account</button>
    <button onClick={() => setToken(current => current === 'fixture' ? 'next-token' : 'fixture')}>Change token</button>
    <button onClick={() => setTenant(current => current === 'fixture' ? 'next-tenant' : 'fixture')}>Change tenant</button>
    <button onClick={() => setGroup(current => current === 'group-first' ? 'group-second' : 'group-first')}>Change group</button>
    <button onClick={() => setExcluded(current => current.length ? [] : ['excluded'])}>Change exclusions</button>
    <button onClick={() => setProtocol(current => current === 'openai' ? 'generation' : 'openai')}>Change protocol</button>
    <UpstreamModelCombobox token={token} tenant={tenant} accountIds={grouped ? [] : [account]} includedProviderGroupIds={grouped ? [group] : []} excludedProviderGroupIds={excluded} syncAccountIds={[account]} upstreams={upstreams} protocol={protocol} value={value} onChange={setValue} customModelConfirmed={parameters.get('confirmed') !== 'false'} existingSelection={parameters.get('existing') !== 'false'} onValidityChange={(next, allowed) => { setValid(next); setCustom(allowed); }} />
    <button disabled={!valid}>Save route</button><output data-custom>{String(custom)}</output>
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
