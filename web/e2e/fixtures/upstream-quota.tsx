import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { UpstreamQuota } from '../../src/operator/UpstreamQuota';
import type { UpstreamQuotaSnapshot } from '../../src/operator/upstreamQuota';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const now = Date.now();
const params = new URLSearchParams(location.search);
const mode = params.get('mode');
const snapshot: UpstreamQuotaSnapshot = {
  contract_version: 'upstream_quota_v1', upstream_account_id: 'quota-account', tenant_external_id: 'default',
  provider: 'openai-codex', status: mode === 'unsupported' ? 'unsupported' : 'ready',
  observed_at: now - 60_000, stale_after: now - 1, stale: true, plan_type: 'Pro',
  credits: { balance: '12.50', unlimited: false, has_credits: true },
  windows: mode === 'unsupported' ? [] : [
    { id: 'primary', label: 'Primary window', used_percent: 75, remaining: 25, limit: 100, reset_at: now + 3600_000, period_seconds: 18000, source: 'provider_usage', reset_is_estimated: false, allowed: true, limit_reached: false },
    { id: 'secondary', label: 'Weekly window', used_percent: null, remaining: null, limit: null, reset_at: null, period_seconds: 604800, source: 'provider_usage', reset_is_estimated: false, allowed: null, limit_reached: null },
  ],
  reset_capability: { provider_supported: mode === 'unsupported' ? false : true, implementation_available: false, available_credits: 2, applicable_credits: 1, reason: 'reset_workflow_not_implemented', credit_error_code: null },
  error_code: null,
};
declare global { interface Window { quotaReads: number; quotaWrites: number } }
window.quotaReads = 0; window.quotaWrites = 0;
window.fetch = async (_input, init) => {
  if (init?.method && init.method !== 'GET') window.quotaWrites += 1;
  window.quotaReads += 1;
  return new Response(JSON.stringify(mode === 'error' ? { error: { message: 'read unavailable' } } : snapshot), { status: mode === 'error' ? 503 : 200 });
};
createRoot(document.getElementById('root')!).render(<I18nProvider><main className="main"><article className="panel provider-list"><div className="account provider-account"><div className="account-main"><b>Quota account</b><UpstreamQuota accountId="quota-account" tenant="default" token="fixture-only" /></div></div></article></main></I18nProvider>);
