import { createRoot } from 'react-dom/client';
import { MtcFluentProvider } from '../../src/design-system';
import { I18nProvider } from '../../src/i18n';
import { UpstreamQuotaDetails } from '../../src/operator/UpstreamQuota';
import type { UpstreamQuotaSnapshot } from '../../src/operator/upstreamQuota';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

window.fetch = async () => { throw new Error('Network forbidden in quota semantics fixture'); };

const resetCapability: UpstreamQuotaSnapshot['reset_capability'] = {
  provider_supported: false, implementation_available: false, prepare_available: false,
  confirmation_required: false, retryable: false, available_credits: null,
  applicable_credits: null, reason: 'supplier_unsupported', credit_error_code: null,
};
const retained: UpstreamQuotaSnapshot = {
  contract_version: 'upstream_quota_v1', upstream_account_id: 'retained', tenant_external_id: 'default', provider: 'openai-codex',
  status: 'ready', observed_at: Date.UTC(2026, 8, 14, 12, 25, 44), stale_after: Date.UTC(2026, 8, 14, 12, 26, 14), stale: true, plan_type: 'Plus',
  credits: { balance: '0', unlimited: false, has_credits: true },
  windows: [{ id: 'code:primary_window', label: 'code:primary_window', used_percent: 0, remaining: null, limit: null, reset_at: null, period_seconds: 18_000, source: 'codex_usage', reset_is_estimated: false, allowed: true, limit_reached: false }],
  reset_capability: resetCapability, error_code: 'quota_transport_failed',
};
const unobserved: UpstreamQuotaSnapshot = {
  ...retained, upstream_account_id: 'unobserved', status: 'error', observed_at: null, stale_after: null, stale: false, plan_type: null,
  // Defensive fixture: inconsistent zero values must remain hidden without a confirmed observation.
  credits: { balance: '0', unlimited: false, has_credits: true },
  windows: [{ ...retained.windows[0], id: 'code:secondary_window', period_seconds: 604_800 }],
};

function Preview() {
  return <main className="main"><article className="panel">
    <h1>Quota semantics · no network</h1>
    <section className="upstream-quota" aria-label="Retained failed refresh" data-case="retained"><UpstreamQuotaDetails snapshot={retained} /></section>
    <section className="upstream-quota" aria-label="Failed refresh without observation" data-case="unobserved"><UpstreamQuotaDetails snapshot={unobserved} /></section>
  </article></main>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Preview /></MtcFluentProvider></I18nProvider>);
