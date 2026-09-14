import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { AuthorizationCodeConnection } from '../../src/operator/AuthorizationCodeConnection';
import { ProvidersPage } from '../../src/operator/pages/ManagementPages';
import type { ProviderType } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const provider: ProviderType = {
  id: 'fixture-plugin', display_name: 'Fixture OAuth', source: 'plugin', protocols: ['openai'], modalities: ['text'],
  config_schema: { type: 'object', properties: { base_url: { type: 'string', default: 'https://api.example.invalid' } } }, credential_schema: {},
  oauth_adapter: { api_version: 'oauth-adapter-v1', flow_kind: 'authorization_code_pkce', login_url: 'https://login.example.invalid', poll_url: '', refresh_url: 'https://token.example.invalid' },
};
function Fixture() {
  const [tenant, setTenant] = useState('fixture-a');
  const [, setLocked] = useState(false);
  const [reads, setReads] = useState(0);
  return <main style={{ maxWidth: 720, margin: 'auto', padding: 16 }}><button onClick={() => setTenant('fixture-b')}>Switch scope</button><output data-testid="account-reads">{reads}</output><AuthorizationCodeConnection key={tenant} token="fixture-token" tenant={tenant} provider={provider} onLock={setLocked} onChanged={async () => {
    const response = await fetch('/internal/v1/upstreams');
    setReads(value => value + 1);
    if (!response.ok) throw new Error('fixture list read failed');
  }} /></main>;
}
function ScopedProvidersFixture() {
  const [tenant, setTenant] = useState('fixture-a');
  const [token, setToken] = useState('fixture-token');
  return <><button onClick={() => setTenant('fixture-b')}>Switch tenant</button><button onClick={() => setToken('next-fixture-token')}>Switch credential</button><ProvidersPage token={token} tenant={tenant} /></>;
}
const params = new URLSearchParams(location.search);
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider>{params.has('scope-controls') ? <ScopedProvidersFixture /> : params.has('full-page') ? <ProvidersPage token="fixture-token" tenant="fixture-a" /> : <Fixture />}</MtcFluentProvider></I18nProvider>);
