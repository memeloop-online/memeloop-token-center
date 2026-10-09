import { api } from '../../api';
import { RequestTable } from '../../components';
import { LoadingProgress, LoadingState, PageLoadingRegion } from '../../design-system';
import { useI18n } from '../../i18n';
import type { OperatorMonitoringSnapshot, RequestView, TypedFilterAst, UpstreamAccount, UsageAnalysisSessionBucket, UsageAnalysisTimeBucket } from '../../types';
import { GenerationWorkspace } from '../GenerationWorkspace';
import { MonitoringSnapshot } from '../MonitoringSnapshot';
import { OverviewUpstreamQuota } from '../OverviewUpstreamQuota';
import { OverviewTrends, useOverviewTrendResource } from '../OverviewTrends';
import { UsageAnalysis } from '../UsageAnalysis';
import { useOperatorResource } from '../hooks/useOperatorResource';
import type { ResourceState } from '../hooks/useOperatorResource';
import type { OperatorRouteKey } from '../scope/operatorRoutes';
import { queryForTenant } from '../scope/operatorShared';

interface OperatorPageProps {
  token: string;
  /** Selected read scope; empty only when no tenant is available. */
  tenant: string;
  /** Explicit target for any mutation rendered by the page. */
  writeTenant?: string;
}

function monitoringSnapshotPath(tenant: string, now: number) {
  const query = new URLSearchParams({
    scope: tenant ? 'tenant' : 'global',
    from_created_at: String(now - 86_400_000),
    to_created_at: String(now),
  });
  if (tenant) query.set('tenant_external_id', tenant);
  return `/internal/v1/monitoring-snapshot?${query}`;
}

function OverviewMonitoringSection({ state, points, token, tenant }: { state: ResourceState<OperatorMonitoringSnapshot>; points?: UsageAnalysisTimeBucket[]; token: string; tenant: string }) {
  const { t } = useI18n();
  if (state.kind === 'idle' || state.kind === 'loading') return <article className="panel"><div className="panel-title"><h2>{t('monitoring.title')}</h2></div>{!(state.kind === 'idle' && state.disabled) && <LoadingState label={t('common.loading')} level="page" variant="detail" />}</article>;
  if (state.kind === 'failed') return <article className="panel"><div className="panel-title"><h2>{t('monitoring.title')}</h2></div><div className="notice error" role="alert">{state.message}</div></article>;
  return <div aria-busy={state.refreshing === true}><LoadingProgress active={state.refreshing === true} label={t('common.loading')} level="page" />{state.refreshError && <div className="notice error" role="alert">{state.refreshError}</div>}<MonitoringSnapshot snapshot={state.value} points={points} quotaSummary={<OverviewUpstreamQuota token={token} tenant={tenant} snapshot={state.value} />} /></div>;
}

function OverviewRecentRequestsSection({ state, onOpenRequest, onOpenSession }: { state: ResourceState<RequestView[]>; onOpenRequest: (requestId: string) => void; onOpenSession: (sessionId: string) => void }) {
  const { t } = useI18n();
  return <article className="panel operator-overview-recent" aria-busy={state.kind === 'loading' || (state.kind === 'ready' && state.refreshing === true)}><div className="panel-title"><h2>{t('self.recent')}</h2><span>{t('sessions.requests')}</span></div>
    {state.kind === 'idle' || state.kind === 'loading'
      ? !(state.kind === 'idle' && state.disabled) && <LoadingState label={t('common.loading')} />
      : state.kind === 'failed'
        ? <div className="notice error" role="alert">{state.message}</div>
        : <><LoadingProgress active={state.refreshing === true} label={t('common.loading')} />{state.refreshError && <div className="notice error" role="alert">{state.refreshError}</div>}<RequestTable requests={state.value} onSelect={(request) => onOpenRequest(request.request_id)} onOpenSession={onOpenSession} /></>}
  </article>;
}

export function OverviewPage({ token, tenant, onNavigate, onOpenRequest, onOpenSession, onRequestDrilldown }: OperatorPageProps & {
  onNavigate: (route: OperatorRouteKey) => void;
  onRequestDrilldown: (ast: TypedFilterAst) => void;
  onOpenUsageSession: (session: UsageAnalysisSessionBucket) => void;
  onOpenRequest: (requestId: string) => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { t } = useI18n();
  const requestResource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    () => api<RequestView[]>(`/internal/v1/requests${queryForTenant(tenant, 'limit=5')}`, token),
    t('common.requestFailed'),
  );
  const monitoringResource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    () => api<OperatorMonitoringSnapshot>(monitoringSnapshotPath(tenant, Date.now()), token),
    t('common.requestFailed'),
  );
  const trends = useOverviewTrendResource(token, tenant);
  return <PageLoadingRegion scopeKey={`${token}\0${tenant}\0overview`} label={t('common.loading')}><div className="operator-overview-dashboard">
    <OverviewMonitoringSection state={monitoringResource.state} token={token} tenant={tenant} points={trends.state.kind === 'ready' ? trends.state.value.time_series : undefined} />
    <OverviewTrends state={trends.state} onDrilldown={(ast) => { onRequestDrilldown(ast); onNavigate('requests'); }} />
    <OverviewRecentRequestsSection state={requestResource.state} onOpenRequest={onOpenRequest} onOpenSession={onOpenSession} />
  </div></PageLoadingRegion>;
}

export function UsagePage({ token, tenant, onOpenSession }: OperatorPageProps & {
  onOpenSession: (session: UsageAnalysisSessionBucket) => void;
}) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    () => api<UpstreamAccount[]>(`/internal/v1/upstreams${tenant ? `?tenant_external_id=${encodeURIComponent(tenant)}` : ''}`, token),
    t('common.requestFailed'),
  );
  const upstreams = resource.state.kind === 'ready' ? resource.state.value : [];
  const upstreamMetadataError = resource.state.kind === 'failed'
    ? resource.state.message
    : resource.state.kind === 'ready' ? resource.state.refreshError : undefined;
  return <PageLoadingRegion scopeKey={`${token}\0${tenant}\0usage`} label={t('common.loading')}>
    {(resource.state.kind === 'loading' || (resource.state.kind === 'idle' && !resource.state.disabled)) && <LoadingState label={t('common.loading')} variant="compact" />}
    <LoadingProgress active={resource.state.kind === 'ready' && resource.state.refreshing === true} label={t('common.loading')} />
    {upstreamMetadataError && <div className="notice error" role="alert">{t('usage.upstreams')}: {upstreamMetadataError}</div>}
    <UsageAnalysis token={token} tenant={tenant} upstreams={upstreams} onOpenSession={onOpenSession} />
  </PageLoadingRegion>;
}

export function GenerationsPage({ token, tenant, writeTenant }: OperatorPageProps) {
  return <GenerationWorkspace token={token} tenant={tenant} writeTenant={writeTenant} />;
}
