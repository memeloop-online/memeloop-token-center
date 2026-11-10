import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system/MtcFluentProvider';
import { HealthProbeSettings } from '../../src/operator/HealthProbeSettings';

const control = {
  writes: [] as { path: string; body: { prompt: string }; release: (fail?: boolean) => void }[],
};
Object.assign(window, { healthProbeFixture: control });
globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url, location.origin);
  if (url.pathname === '/internal/v1/keys') return Response.json([]);
  if (init?.method === 'PUT') {
    const body = JSON.parse(String(init.body));
    return new Promise<Response>(resolve => {
      control.writes.push({
        path: url.pathname + url.search, body,
        release: fail => resolve(fail
          ? Response.json({ error: { message: 'Save failed' } }, { status: 503 })
          : Response.json(body)),
      });
    });
  }
  return Response.json({
    enabled: false, billing_key_id: null, interval_seconds: 3600, model: null,
    prompt: `${url.pathname}:${url.searchParams.get('tenant_external_id')}:${new Headers(init?.headers).get('Authorization')}`,
  });
};

function Fixture() {
  const [account, setAccount] = useState(0);
  const [tenant, setTenant] = useState(0);
  const [token, setToken] = useState(0);
  return <main>
    <button onClick={() => setAccount(value => value + 1)}>Switch account</button>
    <button onClick={() => setTenant(value => value + 1)}>Switch tenant</button>
    <button onClick={() => setToken(value => value + 1)}>Switch token</button>
    <HealthProbeSettings accountId={`account-${account}`} tenant={`tenant-${tenant}`} token={`fixture-${token}`} disabled={false} />
  </main>;
}
createRoot(document.getElementById('root')!).render(
  <I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>,
);
