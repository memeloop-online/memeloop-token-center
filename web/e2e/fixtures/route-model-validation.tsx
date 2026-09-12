import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { UpstreamModelCombobox } from '../../src/operator/UpstreamModelCombobox';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

function Fixture() {
  const [value, setValue] = useState(new URLSearchParams(location.search).get('model') || 'gpt-5.6-luna');
  const [account, setAccount] = useState('first');
  const [valid, setValid] = useState(false);
  const [custom, setCustom] = useState(false);
  return <main style={{ padding: 12 }}>
    <button onClick={() => setAccount('second')}>Change account</button>
    <UpstreamModelCombobox token="fixture" tenant="fixture" accountIds={[account]} includedProviderGroupIds={[]} excludedProviderGroupIds={[]} syncAccountIds={[account]} protocol="openai" value={value} onChange={setValue} customModelConfirmed onValidityChange={(next, allowed) => { setValid(next); setCustom(allowed); }} />
    <button disabled={!valid}>Save route</button><output data-custom>{String(custom)}</output>
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
