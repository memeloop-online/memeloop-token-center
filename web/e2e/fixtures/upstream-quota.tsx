import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { UpstreamQuota } from '../../src/operator/UpstreamQuotaPanel';
import { ProviderAccountStatus } from '../../src/operator/providerAccountStatus';
import { MtcFluentProvider } from '../../src/design-system';
import type { AccountStatusInput, OAuthRefreshStatus } from '../../src/operator/providerAccountState';
import type { UpstreamQuotaSnapshot } from '../../src/operator/upstreamQuota';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const now = Date.now();
const params = new URLSearchParams(location.search);
const mode = params.get('mode');
const recoveryMode = mode?.startsWith('recover') === true;
const snapshot: UpstreamQuotaSnapshot = {
  contract_version: 'upstream_quota_v1', upstream_account_id: 'quota-account', tenant_external_id: 'default',
  provider: mode === 'unsupported' ? 'cursor' : 'openai-codex', status: mode === 'unsupported' ? 'unsupported' : 'ready',
  observed_at: mode === 'unsupported' ? null : now - 60_000, stale_after: mode === 'unsupported' ? null : now - 1,
  stale: mode !== 'unsupported', freshness: mode === 'unsupported' ? 'unobserved' : 'stale', plan_type: mode === 'unsupported' ? null : 'Pro',
  workspace: null,
  capabilities: { read: mode !== 'unsupported', plan: mode !== 'unsupported', workspace: false, window_amounts: false, window_amount_unit: false, window_percent: mode !== 'unsupported', reset_credit_expiry: mode !== 'unsupported', subscription_expiry: false, supplier_read_only: true, refreshes_credentials: false, consumes_reset_credit: false },
  subscription_active_until: null,
  credits: { balance: mode === 'unsupported' ? null : '12.50', unlimited: mode === 'unsupported' ? null : false, has_credits: mode === 'unsupported' ? null : true, source: mode === 'unsupported' ? null : 'codex_usage' },
  windows: mode === 'unsupported' ? [] : [
    { id: 'code:primary_window', label: 'code:primary_window', used_percent: 75, used: null, remaining: 25, limit: 100, unit: null, reset_at: now + 3600_000, period_seconds: 18000, source: 'codex_usage', reset_is_estimated: false, allowed: true, limit_reached: false },
    { id: 'code:secondary_window', label: 'code:secondary_window', used_percent: null, used: null, remaining: null, limit: null, unit: null, reset_at: null, period_seconds: 604800, source: 'codex_usage', reset_is_estimated: false, allowed: null, limit_reached: null },
  ],
  reset_capability: { provider_supported: mode === 'unsupported' ? null : true, implementation_available: mode === 'reset' || mode === 'unknown' || mode === 'prepared-current', prepare_available: mode === 'reset' || mode === 'unknown' || mode === 'prepared-current', confirmation_required: mode === 'reset' || mode === 'unknown' || mode === 'prepared-current', retryable: false, available_credits: mode === 'unsupported' ? null : 2, applicable_credits: mode === 'unsupported' ? null : 1, reason: mode === 'unsupported' ? 'quota_reset_not_supported' : 'explicit_confirmation_required', credit_error_code: null, evidence: mode === 'unsupported' ? 'unknown_provider' : 'server_driver_contract' },
  reset_credits: [],
  error_code: mode === 'stale-error' ? 'quota_destination_invalid' : mode === 'rate-limited' ? 'quota_rate_limited' : null,
};
if (recoveryMode) snapshot.reset_capability = { ...snapshot.reset_capability, implementation_available: true, prepare_available: true };
if (mode === 'kimi') {
  snapshot.provider = 'kimi-oauth'; snapshot.stale = false; snapshot.stale_after = now + 300_000;
  snapshot.freshness = 'fresh'; snapshot.capabilities = { ...snapshot.capabilities, plan: false, window_amounts: true, reset_credit_expiry: false };
  snapshot.reset_capability = { ...snapshot.reset_capability, provider_supported: false, implementation_available: false, prepare_available: false, confirmation_required: false, available_credits: null, applicable_credits: null };
  snapshot.windows = snapshot.windows.map((window, index) => ({ ...window, id: index ? 'summary' : 'limit-0', label: index ? 'summary' : 'limit-0', source: 'kimi_usage', unit: null, used_percent: index ? 25 : 0, remaining: 100, limit: 100, reset_at: now + (index ? 604_800_000 : 18_000_000) }));
  if (params.has('units')) snapshot.windows.push(...['requests', 'tokens', 'Vendor Compute Units'].map(unit => ({ ...snapshot.windows[0], id: unit, label: unit, period_seconds: null, reset_at: null, used_percent: null, remaining: 25, limit: 100, unit })));
}
declare global { interface Window { quotaReads: number; quotaWrites: number; quotaPrepares: number; quotaConfirms: number; quotaStatuses: number; quotaReconciles: number; quotaCurrents: number } }
window.quotaReads = 0; window.quotaWrites = 0;
window.quotaPrepares = 0; window.quotaConfirms = 0;
window.quotaStatuses = 0; window.quotaReconciles = 0; window.quotaCurrents = 0;
let operation = { id: 'mock-operation', upstream_account_id: 'quota-account', state: recoveryMode ? 'unknown' : 'prepared', expires_at: now + 120_000, effect: 'supplier_defined_codex_rate_limits', consumes_credits: 1, last_reconciled_at: null as number | null, reconciled_available_credits: null as number | null, reconciled_applicable_credits: null as number | null, settled_at: mode === 'recover-released' ? now : null as number | null, updated_at: now, manual_unlock_available: recoveryMode && mode !== 'recover-released', quota_refresh_status: 'not_attempted' };
window.fetch = async (_input, init) => {
  const url = new URL(String(_input), location.origin);
  const method = init?.method ?? 'GET';
  const quota = '/internal/v1/upstreams/quota-account/quota';
  const reset = '/internal/v1/upstreams/quota-account/quota-reset/';
  if (url.origin !== location.origin || url.searchParams.get('tenant_external_id') !== 'default'
    || (url.pathname === quota && (url.searchParams.get('fresh') !== 'true' || url.searchParams.get('trigger') !== 'manual'))
    || !((url.pathname === quota && method === 'GET')
      || (url.pathname === `${reset}prepare` && method === 'POST')
      || (url.pathname === `${reset}current` && method === 'GET')
      || (url.pathname === `${reset}${operation.id}` && method === 'GET')
      || (['confirm', 'reconcile', 'recover'].some((action) => url.pathname === `${reset}${operation.id}/${action}`) && method === 'POST'))) {
    throw new Error('Unexpected request in mock-only quota fixture');
  }
  if (init?.method && init.method !== 'GET') window.quotaWrites += 1;
  if (url.pathname === quota) window.quotaReads += 1;
  const path = String(_input);
  if (url.pathname === `${reset}current`) {
    window.quotaCurrents += 1;
    return new Response(mode === 'prepared-current' || recoveryMode ? JSON.stringify(operation) : 'null');
  }
  if (path.includes('/quota-reset/')) {
    if (path.includes('/prepare')) {
      window.quotaPrepares += 1;
      if (recoveryMode) operation = { ...operation, id: 'new-operation', state: 'prepared', settled_at: null, manual_unlock_available: false };
      return new Response(JSON.stringify({ operation, confirmation_token: 'mock-confirmation-not-a-real-credential' }));
    }
    if (path.includes('/confirm')) {
      const body = JSON.parse(String(init?.body));
      if (body.confirmation !== 'consume_one_supplier_reset_credit' || body.confirmation_token !== 'mock-confirmation-not-a-real-credential') throw new Error('Invalid mock confirmation');
      window.quotaConfirms += 1;
      operation = { ...operation, state: mode === 'unknown' ? 'unknown' : 'accepted', quota_refresh_status: recoveryMode ? 'pending' : 'not_attempted' };
    }
    if (path.includes('/recover')) {
      const body = JSON.parse(String(init?.body));
      if (Object.keys(body).sort().join(',') !== 'expected_updated_at,reason' || body.expected_updated_at !== operation.updated_at || !['operator_authorized_retry', 'supplier_confirmed_not_applied'].includes(body.reason)) throw new Error('Invalid recovery contract');
      if (mode === 'recover-stale') return new Response('{}', { status: 409 });
      if (mode === 'recover-forbidden') return new Response('{}', { status: 403 });
      if (mode === 'recover-failed') return new Response('{}', { status: 503 });
      operation = { ...operation, settled_at: now, updated_at: now + 1, manual_unlock_available: false, last_reconciled_at: now, reconciled_available_credits: 1, reconciled_applicable_credits: 0 };
    }
    if (path.includes('/reconcile')) { window.quotaReconciles += 1; operation = { ...operation, last_reconciled_at: Date.now(), reconciled_available_credits: 1, reconciled_applicable_credits: 0 }; }
    if (method === 'GET') window.quotaStatuses += 1;
    return new Response(JSON.stringify(operation));
  }
  const denied = mode === 'permission';
  return new Response(JSON.stringify(mode === 'error' || denied ? { error: { message: 'fixture-sensitive-message-must-not-render' } } : snapshot), { status: denied ? 403 : mode === 'error' ? 503 : 200 });
};
const renewal: OAuthRefreshStatus = { credential_generation: 1, access_state: 'expired', refresh_state: 'outcome_unknown', failure_class: 'network', reauthorization_required: false, automatic_refresh_paused: true };
const stateAccount: AccountStatusInput = { status: 'active', auth_kind: 'oauth', credential_generation: 1, credential_expires_at: now - 1000 };
const states: [string, AccountStatusInput][] = [
  ['0142', { ...stateAccount, oauth_refresh: renewal }],
  ['7021', { ...stateAccount, oauth_refresh: { ...renewal, failure_class: 'local_persistence' } }],
  ['revoked', { ...stateAccount, oauth_refresh: { ...renewal, refresh_state: 'reauthorization_required', reauthorization_required: true, access_state: 'revoked' } }],
  ['missing', { ...stateAccount, credential_expires_at: null }],
];
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><main className="main"><article className="panel provider-list">
  {recoveryMode && <section aria-label="Kimi">{states.map(([name, account]) => <p key={name} data-account-state={name}>{name} · <ProviderAccountStatus account={account} /></p>)}</section>}
  <div className="account provider-account"><div className="account-main"><b>Quota account</b><UpstreamQuota accountId="quota-account" credentialGeneration={1} tenant="default" token="fixture-only" canRecover={recoveryMode && mode !== 'recover-tenant'} /></div></div>
</article></main></MtcFluentProvider></I18nProvider>);
