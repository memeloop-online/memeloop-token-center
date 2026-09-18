import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider, useI18n } from '../../src/i18n';
import { QuotaResetCreditExpiry, UpstreamQuota } from '../../src/operator/UpstreamQuotaPanel';
import { useUpstreamQuotaReads } from '../../src/operator/useUpstreamQuotaReads';
import { quotaSummaryPresentation, type UpstreamQuotaSnapshot } from '../../src/operator/upstreamQuota';
import '../../src/styles.css';
import '../../src/theme.css';
declare global { interface Window { quotaReadCalls: string[]; quotaReadTriggers: string[]; quotaActive: number; quotaPeak: number; quotaUnexpectedWrites: number; releaseQuota: (index: number, status?: number) => void } }
window.quotaReadCalls = []; window.quotaActive = 0; window.quotaPeak = 0; window.quotaUnexpectedWrites = 0;
window.quotaReadTriggers = [];
const pending: ((status: number) => void)[] = [];
window.releaseQuota = (index, status = 200) => pending[index](status);
window.fetch = async (input, init) => {
  const url = new URL(String(input), location.origin);
  if ((init?.method ?? 'GET') !== 'GET' || !/^\/internal\/v1\/upstreams\/account-\d\/quota$/.test(url.pathname) || url.searchParams.get('fresh') !== 'true' || !['manual', 'bulk'].includes(url.searchParams.get('trigger') ?? '')) { window.quotaUnexpectedWrites++; throw new Error('Only explicit fresh mock quota GET is allowed'); }
  const id = url.pathname.split('/')[4];
  window.quotaReadCalls.push(id); window.quotaReadTriggers.push(url.searchParams.get('trigger')!); window.quotaActive++; window.quotaPeak = Math.max(window.quotaPeak, window.quotaActive);
  const status = await new Promise<number>(resolve => pending.push(resolve));
  window.quotaActive--;
  const now = Date.now();
  const snapshot: UpstreamQuotaSnapshot = {
    contract_version: 'upstream_quota_v1', upstream_account_id: id, tenant_external_id: url.searchParams.get('tenant_external_id')!, provider: 'openai-codex', status: 'ready', observed_at: now, stale_after: now + 60_000, stale: false, freshness: 'fresh', plan_type: null, workspace: null,
    capabilities: { read: true, plan: true, workspace: false, window_amounts: false, window_amount_unit: false, window_percent: true, reset_credit_expiry: true, subscription_expiry: false, supplier_read_only: true, refreshes_credentials: false, consumes_reset_credit: false }, subscription_active_until: null,
    credits: { balance: null, unlimited: null, has_credits: null, source: null }, windows: [], error_code: null,
    reset_credits: [{ status: 'available', granted_at: now - 60_000, expires_at: id === 'account-1' ? null : now + 86400_000, source: 'codex_reset_credits' }],
    reset_capability: { provider_supported: true, implementation_available: false, prepare_available: false, confirmation_required: true, retryable: false, available_credits: 1, applicable_credits: 1, reason: 'quota_reset_not_supported', credit_error_code: null, evidence: 'server_driver_contract' },
  };
  return new Response(JSON.stringify(status === 200 ? snapshot : { error: { code: 'test_unavailable', message: 'fixture failure' } }), { status });
};
function Fixture() {
  const { t } = useI18n();
  const [generation, setGeneration] = useState(1);
  const [tenant, setTenant] = useState('default');
  const [disabled, setDisabled] = useState<string[]>([]);
  const accounts = Array.from({ length: 5 }, (_, index) => ({ id: `account-${index}`, credential_generation: generation, tenant_external_id: tenant, status: disabled.includes(`account-${index}`) ? 'disabled' : 'active' }));
  const quota = useUpstreamQuotaReads('fixture-only', tenant, accounts);
  return <main><button onClick={() => void quota.readAll()} disabled={quota.progress?.busy}>{t('quota.refreshAll')}</button><button onClick={() => { void quota.read(accounts[0]); void quota.read(accounts[0]); }}>Duplicate read</button><button onClick={() => setGeneration(value => value + 1)}>Change generation</button><button onClick={() => setTenant('other')}>Change tenant</button><button onClick={() => setDisabled(['account-0'])}>Disable first</button><button onClick={() => setDisabled(['account-1'])}>Disable second</button><output>{quota.progress ? `${quota.progress.done}/${quota.progress.total}` : 'idle'}</output>
    {accounts.map(account => {
      const entry = quota.entries[account.id]?.generation === generation ? quota.entries[account.id] : undefined;
      const summary = quotaSummaryPresentation(entry?.snapshot, Date.now(), entry?.refreshFailed);
      return <section key={account.id} data-account={account.id}><h2>{account.id}</h2><span data-summary>{t(entry?.busy ? 'quota.refreshing' : entry?.queued ? 'quota.queued' : summary.key, { percent: '—' })}</span>{entry?.snapshot && <QuotaResetCreditExpiry snapshot={entry.snapshot} />}<button onClick={() => void quota.read(account)} disabled={Boolean(entry?.busy || quota.progress?.busy)}>{t('quota.refreshAccount')}</button><UpstreamQuota accountId={account.id} credentialGeneration={generation} tenant={tenant} token="fixture-only" readState={entry} onRefresh={() => void quota.read(account)} refreshDisabled={quota.progress?.busy} /></section>;
    })}</main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
