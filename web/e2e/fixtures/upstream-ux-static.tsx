import { useEffect } from 'react';
import Form from '@rjsf/core/lib/components/Form.js';
import { createRoot } from 'react-dom/client';
import { I18nProvider, useI18n } from '../../src/i18n';
import { UpstreamQuotaDetails } from '../../src/operator/UpstreamQuota';
import { UpstreamQuotaReset } from '../../src/operator/UpstreamQuotaReset';
import { UpstreamConnection, connectionSchema } from '../../src/operator/UpstreamConnection';
import { upstreamFormTemplates } from '../../src/operator/UpstreamFormTemplates';
import { safeValidator } from '../../src/safeValidator';
import type { UpstreamAccount } from '../../src/types';
import { useConfirmDialog } from '../../src/useConfirmDialog';
import type { UpstreamQuotaSnapshot } from '../../src/operator/upstreamQuota';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';
import '../../src/operator/upstreamConnection.css';

// Static visual acceptance: no quota loader and no mutation handler is activated.
window.fetch = async () => { throw new Error('Network forbidden in static acceptance'); };
const snapshot: UpstreamQuotaSnapshot = {
  contract_version: 'upstream_quota_v1', upstream_account_id: 'mock-account', tenant_external_id: 'default', provider: 'openai-codex',
  status: 'ready', observed_at: Date.now() - 120000, stale_after: Date.now() - 1, stale: true, plan_type: 'Plus',
  credits: { balance: null, unlimited: false, has_credits: true },
  windows: [
    { id: 'weekly', label: 'Weekly', used_percent: 83, remaining: null, limit: null, reset_at: Date.now() + 3600000, period_seconds: 604800, source: 'static mock', reset_is_estimated: true, allowed: true, limit_reached: false },
    { id: 'unknown', label: 'Unknown usage', used_percent: null, remaining: null, limit: null, reset_at: null, period_seconds: null, source: 'static mock', reset_is_estimated: false, allowed: null, limit_reached: null },
  ],
  reset_capability: { provider_supported: true, implementation_available: true, prepare_available: true, confirmation_required: true, retryable: false, available_credits: 2, applicable_credits: 1, reason: null, credit_error_code: null }, error_code: null,
};
const account = { id: 'mock-account', driver: 'openai-codex', auth_kind: 'oauth', config: { base_url: 'https://chatgpt.com/backend-api/codex' }, has_proxy: true, proxy_scheme: 'socks5h', proxy_remote_dns: true, proxy_fingerprint: 'proxy_mock_redacted', can_update_transport_proxy: true, credential_generation: 1, updated_at: 1 } as UpstreamAccount;
const config = connectionSchema({ type: 'object', properties: { base_url: { type: 'string', const: account.config.base_url }, transport_policy: { type: 'object', properties: { connect_attempts: { type: 'integer', minimum: 1, maximum: 4, default: 2 } } } } }, 'Fixed API endpoint; proxy is configured separately.');
function Preview() {
  const { t } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog(['static']);
  useEffect(() => {
    if (new URLSearchParams(location.search).has('confirm')) void confirm(t('quota.resetConfirm', { account: 'Mock Codex', id: 'mock-account', expiry: '2026-09-11 12:00 UTC' }));
  }, []);
  return <main className="main"><article className="panel"><h1>Static quota acceptance · no network</h1><UpstreamConnection account={account} token="mock-only" tenant="default" disabled={false} onChanged={async () => {}} /><Form schema={{ type: 'object', properties: { name: { type: 'string', title: 'Upstream name' }, config } }} formData={{ name: 'Mock Codex', config: account.config }} validator={safeValidator} templates={upstreamFormTemplates}><span /></Form><section className="upstream-quota"><h2>{t('quota.title')}</h2><p className="notice error" role="alert">{t('quota.refreshFailedRetained')}</p><UpstreamQuotaDetails snapshot={snapshot} /><details className="upstream-danger-zone" open><summary>{t('quota.resetAction')}</summary><p>{t('quota.resetWarning')}</p><UpstreamQuotaReset accountId="mock-account" accountName="Mock Codex" tenant="default" token="mock-only" snapshot={snapshot} /></details></section>{confirmationDialog}</article></main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Preview /></I18nProvider>);
