import { useEffect, useMemo } from 'react';
import { Button } from '@fluentui/react-components';
import { api } from '../api';
import { useI18n } from '../i18n';
import type { OperatorMonitoringSnapshot, UpstreamAccount } from '../types';
import { AnalyticsMetric } from './AnalyticsMetric';
import { QuotaSummary } from './QuotaSummary';
import { useOperatorResource } from './hooks/useOperatorResource';
import { queryForTenant } from './scope/operatorShared';
import { useUpstreamQuotaReads } from './useUpstreamQuotaReads';
import { upstreamDisplayName } from '../identityPresentation.js';

/** Only accounts already represented in the bounded monitoring snapshot. */
export function OverviewUpstreamQuota({ token, tenant, snapshot }: {
  token: string; tenant: string; snapshot: OperatorMonitoringSnapshot;
}) {
  const { t } = useI18n();
  const names = useMemo(() => new Map(snapshot.top_upstream_models.map(account => [account.upstream_account_id, account.upstream_name])), [snapshot.top_upstream_models]);
  const resource = useOperatorResource(Boolean(token && names.size), `${token}\0${tenant}`, signal => api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token, { signal }), t('common.requestFailed'));
  const accounts = useMemo(() => resource.state.kind === 'ready' ? resource.state.value.filter(account => names.has(account.id)) : [], [resource.state, names]);
  const reads = useUpstreamQuotaReads(token, tenant, accounts);
  const signature = JSON.stringify(accounts.map(account => [account.id, account.credential_generation, account.tenant_external_id, account.status]));
  useEffect(() => { void reads.readAll(); }, [token, tenant, signature]);
  if (!names.size) return null;
  return <section className="overview-upstream-quota" aria-label={t('monitoring.upstreamQuota')}>
    <div className="panel-title"><div><h2>{t('monitoring.upstreamQuota')}</h2><p className="muted">{t('monitoring.upstreamQuotaScope')}</p></div>
      <Button appearance="subtle" disabled={!accounts.length || Boolean(reads.progress?.busy)} onClick={() => void reads.readAll()}>{t(reads.progress?.busy ? 'common.loading' : 'quota.refresh')}</Button>
    </div>
    {resource.state.kind === 'failed' && <p role="alert">{resource.state.message}</p>}
    <div className="metrics overview-quota-metrics">{Array.from(names, ([id, name]) => {
      const account = accounts.find(account => account.id === id);
      const candidate = reads.entries[id];
      const state = candidate?.generation === account?.credential_generation ? candidate : undefined;
      return <AnalyticsMetric key={id} label={name === id ? t('monitoring.unnamedUpstream') : upstreamDisplayName(name, t)}
        value={state?.busy && !state.snapshot ? t('common.loading') : <QuotaSummary mode="remaining" snapshot={state?.snapshot} refreshFailed={Boolean(state?.refreshFailed)} />}
        note={state?.error ? t(state.error) : undefined} />;
    })}</div>
  </section>;
}
