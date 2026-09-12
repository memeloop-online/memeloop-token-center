import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import type { RJSFSchema } from '@rjsf/utils';
import { I18nProvider } from '../../src/i18n';
import { OperatorSchemaForm } from '../../src/operator/pages/ManagementPages';
import { ProxyInput } from '../../src/operator/UpstreamConnection';
import { safeValidator } from '../../src/safeValidator';
import { schemaFormTemplates } from '../../src/SchemaTemplates';
import { upstreamFormTemplates } from '../../src/operator/UpstreamFormTemplates';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

// Synthetic only: no provider APIs, no secrets in logs, response bodies or DOM.
window.fetch = async () => { throw new Error('Network forbidden in secret-input fixture'); };
const variant = new URLSearchParams(location.search).get('variant') ?? 'api';
const secret = { type: 'string', writeOnly: true, default: 'must-not-prefill', examples: ['must-not-suggest'] } as const;
const credential: RJSFSchema = variant === 'oauth' ? {
  type: 'object', required: ['access_token', 'refresh_token', 'adapter_state'], properties: {
    access_token: { ...secret, title: 'Access token' },
    refresh_token: { ...secret, title: 'Refresh token' },
    adapter_state: { type: 'object', writeOnly: true, title: 'Adapter state', default: { forbidden: true } },
  },
} : variant === 'plugin' ? {
  type: 'object', required: ['password', 'client_secret'], properties: {
    password: { type: 'string', format: 'password', title: 'Password' },
    client_secret: { $ref: '#/$defs/secret' },
  },
} : {
  oneOf: [{ title: 'API key', type: 'object', required: ['value'], properties: {
    value: { ...secret, title: 'Credential value' },
    proxy_url: { type: ['string', 'null'], writeOnly: true, title: 'Proxy URL' },
  } }, { title: 'No authentication', type: 'object', properties: {} }],
};
const definitions: RJSFSchema['$defs'] = { secret: { ...secret, title: 'Client secret' } };

function Fixture() {
  const [submitted, setSubmitted] = useState(0);
  const [proxy, setProxy] = useState('');
  const [generation, setGeneration] = useState(0);
  return <main className="main">
    <button type="button" onClick={() => setGeneration((value) => value + 1)}>Reopen forms</button>
    <p role="status">Successful synthetic submissions: {submitted}</p>
    <section className="panel" aria-label="Create credential">
      <h1>Create credential</h1>
      <OperatorSchemaForm key={`create-${generation}`} schema={{ type: 'object', $defs: definitions, required: ['credential'], properties: { credential } }} validator={safeValidator} templates={upstreamFormTemplates} onSubmit={() => setSubmitted((count) => count + 1)}><button type="submit">Create fixture</button></OperatorSchemaForm>
    </section>
    <section className="panel" aria-label="Rotate credential">
      <h2>Rotate credential</h2>
      <OperatorSchemaForm key={`rotate-${generation}`} schema={{ ...credential, $defs: definitions }} validator={safeValidator} templates={schemaFormTemplates} onSubmit={() => setSubmitted((count) => count + 1)}><button type="submit">Rotate fixture</button></OperatorSchemaForm>
    </section>
    <section className="panel" aria-label="Edit connection">
      <h2>Edit connection</h2>
      <OperatorSchemaForm schema={{ type: 'object', properties: { name: { type: 'string' }, config: { type: 'object', properties: { base_url: { type: 'string' } } } } }} formData={{ name: 'Existing account', config: { base_url: 'https://provider.example' } }} validator={safeValidator} templates={upstreamFormTemplates}><span /></OperatorSchemaForm>
    </section>
    <section className="panel" aria-label="Account proxy"><ProxyInput value={proxy} onChange={setProxy} /></section>
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
