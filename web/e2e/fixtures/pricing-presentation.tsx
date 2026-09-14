import React, { useState } from 'react';
import { MtcFluentProvider } from '../../src/design-system';
import { PricingPage } from '../../src/operator/pages/ManagementPages';
import { PricingTable } from '../../src/operator/PricingTable';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { CredentialPolicySummary } from '../../src/operator/CredentialPolicySummary';
import { PriceSource } from '../../src/operator/PriceSource';
import '../../src/styles.css';
import '../../src/operator/operator.css';
import '../../src/modelPicker.css';
import '../../src/theme.css';

const policy = { requests_per_minute: 4294967295, tokens_per_minute: 9007199254740991, max_concurrency: 4294967295, daily_budget: null, weekly_budget: '1234567', lifetime_budget: null };
const workspace = new URLSearchParams(location.search).has('workspace');
const tableStates = new URLSearchParams(location.search).has('table-states');
function TableStates() {
  const [usageState, setUsageState] = useState('ready');
  const [loading, setLoading] = useState(false);
  return <main className="pricing-page" style={{ padding: 16 }}>
    <button onClick={() => setUsageState('loading')}>Reload usage</button>
    <button onClick={() => setUsageState('failed')}>Reject usage</button>
    <button onClick={() => setLoading(true)}>Reload prices</button>
    <PricingTable currency="USD" loading={loading} usageLoading={usageState === 'loading'} usageFailed={usageState === 'failed'} rows={[
      { model: 'used-model', usage: { model: 'used-model', calls: 7, input_tokens: 0, output_tokens: 0 } },
      { model: 'unused-model' },
    ]} />
  </main>;
}
let releaseUsage: ((response: Response) => void) | undefined;
const respond = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });
if (workspace) {
  window.fetch = async (input, options) => {
    const path = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin).pathname;
    if (path.endsWith('/usage-summary')) return new Promise<Response>(resolve => { releaseUsage = resolve; });
    if (path === '/internal/v1/model-prices') return respond(['active-model', 'unused-model'].map(model => ({
      model, currency: 'USD', input_per_million: '2', output_per_million: '8', source: 'models.dev', updated_at: 1000,
      tiers: [{ service_tier: 'default', input_per_million: '2', cached_input_per_million: '0.2', cache_write_per_million: '2', output_per_million: '8', source: 'models.dev', updated_at: 1000, cache_price_estimated: false }],
    })));
    if (path === '/internal/v1/generation-prices') return respond([]);
    if (path === '/internal/v1/schemas') return respond({
      model_price: { type: 'object', required: ['input_per_million', 'output_per_million'], properties: { service_tier: { type: 'string', default: 'default', enum: ['default', 'priority', 'flex'] }, input_per_million: { type: 'string', title: 'Input / million tokens', pattern: '^[0-9]+(\\.[0-9]+)?$' }, output_per_million: { type: 'string', title: 'Output / million tokens', pattern: '^[0-9]+(\\.[0-9]+)?$' } } },
      generation_price: { type: 'object', properties: { price_per_unit: { type: 'string', title: 'Unit price' } } },
    });
    if (options?.method === 'POST' && path.startsWith('/internal/v1/prices/')) {
      if (JSON.parse(String(options.body)).service_tier !== 'default') return respond({ error: { code: 'fixture_invalid_tier', message: 'Localized labels must preserve the default API value' } }, 400);
      return respond({ error: { code: 'conflict', message: 'Fixture price rejected; draft retained' } }, 409);
    }
    return respond({ error: { code: 'fixture_unexpected_request', message: path } }, 400);
  };
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider>{tableStates ? <TableStates /> : workspace ? <main style={{ padding: 16 }}>
  <button onClick={() => releaseUsage?.(respond({ models: [{ model: 'active-model', calls: 7, input_tokens: 100, output_tokens: 20 }, { model: 'missing-model', calls: 2, input_tokens: 20, output_tokens: 5 }] }))}>Complete usage</button>
  <button onClick={() => releaseUsage?.(respond({ error: { code: 'unavailable', message: 'Fixture usage unavailable' } }, 503))}>Fail usage</button>
  <PricingPage token="fixture-operator" tenant="fixture" writeTenant="fixture" />
</main> : <main style={{ padding: 16 }}>
  <section aria-label="Unlimited credential"><CredentialPolicySummary currency="USD" policy={{ ...policy, enforcement_mode: 'metered_unlimited' }} /></section>
  <section aria-label="Prepaid credential"><CredentialPolicySummary currency="USD" policy={{ ...policy, enforcement_mode: 'prepaid' }} /></section>
  <PriceSource source={'cpamp:import-run/' + 'a'.repeat(160)} />
  <PriceSource source="copied:old-price" />
  <PriceSource source="models.dev" />
</main>}</MtcFluentProvider></I18nProvider>);
