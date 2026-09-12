import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { UpstreamQuotaResetSection } from '../../src/operator/UpstreamQuota';
import type { UpstreamQuotaSnapshot } from '../../src/operator/upstreamQuota';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

// Static failure-state presentation only. Any fetch is a fixture failure.
window.fetch = async () => { throw new Error('Quota discovery fixture must never request any endpoint'); };
const snapshot: UpstreamQuotaSnapshot = {
  contract_version: 'upstream_quota_v1', upstream_account_id: 'fixture', tenant_external_id: 'fixture',
  provider: 'openai-codex', status: 'error', observed_at: null, stale_after: null, stale: true,
  plan_type: null, credits: { balance: null, unlimited: null, has_credits: null }, windows: [], error_code: 'connection_failed',
  reset_capability: { provider_supported: true, implementation_available: true, prepare_available: true,
    confirmation_required: true, retryable: true, available_credits: null, applicable_credits: null,
    reason: 'quota_refresh_failed_retryable', credit_error_code: null },
};
const props = { accountId: 'fixture', accountName: 'Fixture account', tenant: 'fixture', token: 'fixture-only' };
createRoot(document.getElementById('root')!).render(<I18nProvider><main style={{ padding: 12 }}>
  <section data-state="pending"><UpstreamQuotaResetSection {...props} /></section>
  <section data-state="failed"><UpstreamQuotaResetSection {...props} readFailed /></section>
  <section data-state="snapshot"><UpstreamQuotaResetSection {...props} snapshot={snapshot} /></section>
</main></I18nProvider>);
