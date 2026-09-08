import { api } from '../../api';
import { RequestTable } from '../../components';
import { useI18n } from '../../i18n';
import type { OperatorMonitoringSnapshot, PluginManifest, RequestView, UpstreamAccount, UsageAnalysisSessionBucket } from '../../types';
import { GenerationWorkspace } from '../GenerationWorkspace';
import { MonitoringSnapshot } from '../MonitoringSnapshot';
import { OverviewTrends } from '../OverviewTrends';
import { Plugins } from '../Plugins';
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

function OverviewMonitoringSection({ state }: { state: ResourceState<OperatorMonitoringSnapshot> }) {
  const { t } = useI18n();
  if (state.kind === 'idle' || state.kind === 'loading') return <article className="panel"><div className="panel-title"><h2>{t('monitoring.title')}</h2></div><div className="empty">{t('common.loading')}</div></article>;
  if (state.kind === 'failed') return <article className="panel"><div className="panel-title"><h2>{t('monitoring.title')}</h2></div><div className="notice error" role="alert">{state.message}</div></article>;
  return <>{state.refreshError && <div className="notice error" role="alert">{state.refreshError}</div>}<MonitoringSnapshot snapshot={state.value} /></>;
}

function OverviewRecentRequestsSection({ state, onOpenSession }: { state: ResourceState<RequestView[]>; onOpenSession: (sessionId: string) => void }) {
  const { t } = useI18n();
  return <article className="panel operator-overview-recent"><div className="panel-title"><h2>{t('self.recent')}</h2><span>{t('sessions.requests')}</span></div>
    {state.kind === 'idle' || state.kind === 'loading'
      ? <div className="empty">{t('common.loading')}</div>
      : state.kind === 'failed'
        ? <div className="notice error" role="alert">{state.message}</div>
        : <>{state.refreshError && <div className="notice error" role="alert">{state.refreshError}</div>}<RequestTable requests={state.value} onOpenSession={onOpenSession} /></>}
  </article>;
}

export function OverviewPage({ token, tenant, onNavigate, onOpenSession }: OperatorPageProps & {
  onNavigate: (route: OperatorRouteKey) => void;
  onOpenUsageSession: (session: UsageAnalysisSessionBucket) => void;
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
  const destinations: Array<{ route: OperatorRouteKey; label: string }> = [
    { route: 'requests', label: t('nav.traffic') },
    { route: 'sessions', label: t('sessions.sessionsMode') },
    { route: 'usage', label: t('nav.usage') },
    { route: 'providers', label: t('nav.providers') },
    { route: 'routes', label: t('nav.routes') },
    { route: 'credentials', label: t('nav.credentials') },
  ];
  return <div className="operator-overview-dashboard">
    <article className="panel operator-overview-shortcuts"><div className="panel-title"><div><h2>{t('usage.overview')}</h2><p className="muted">{t('operator.subtitle')}</p></div></div><div className="row-actions">{destinations.map((item) => <button type="button" className="secondary" key={item.route} onClick={() => onNavigate(item.route)}>{item.label}</button>)}</div></article>
    <OverviewMonitoringSection state={monitoringResource.state} />
    <OverviewTrends token={token} tenant={tenant} />
    <OverviewRecentRequestsSection state={requestResource.state} onOpenSession={onOpenSession} />
  </div>;
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
  return <>
    {(resource.state.kind === 'idle' || resource.state.kind === 'loading') && <div className="notice" role="status">{t('usage.upstreams')}: {t('common.loading')}</div>}
    {upstreamMetadataError && <div className="notice error" role="alert">{t('usage.upstreams')}: {upstreamMetadataError}</div>}
    <UsageAnalysis token={token} tenant={tenant} upstreams={upstreams} onOpenSession={onOpenSession} />
  </>;
}

export function GenerationsPage({ token, tenant, writeTenant }: OperatorPageProps) {
  return <GenerationWorkspace token={token} tenant={tenant} writeTenant={writeTenant} />;
}

export function PluginsPage({ token, tenant, writeTenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<PluginManifest[]>('/internal/v1/plugins', token),
    t('common.requestFailed'),
  );
  if (resource.state.kind === 'idle' || resource.state.kind === 'loading') return <div className="empty">{t('common.loading')}</div>;
  if (resource.state.kind === 'failed') return <div className="notice error" role="alert">{resource.state.message}</div>;
  return <>{resource.state.refreshError && <div className="notice error" role="alert">{resource.state.refreshError}</div>}<Plugins token={token} tenant={tenant} writeTenant={writeTenant} values={resource.state.value} /></>;
}
