import { api } from '../../api';
import { RequestTable } from '../../components';
import { useI18n } from '../../i18n';
import type { PluginManifest, RequestView, UpstreamAccount, UsageAnalysisSessionBucket } from '../../types';
import { GenerationWorkspace } from '../GenerationWorkspace';
import { Plugins } from '../Plugins';
import { UsageAnalysis } from '../UsageAnalysis';
import { useOperatorResource } from '../hooks/useOperatorResource';
import type { OperatorRouteKey } from '../scope/operatorRoutes';
import { queryForTenant } from '../scope/operatorShared';

interface OperatorPageProps { token: string; tenant: string }

export function OverviewPage({ token, tenant, onNavigate, onOpenUsageSession, onOpenSession }: OperatorPageProps & {
  onNavigate: (route: OperatorRouteKey) => void;
  onOpenUsageSession: (session: UsageAnalysisSessionBucket) => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    async () => {
      const [upstreams, requests] = await Promise.all([
        api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token),
        api<RequestView[]>(`/internal/v1/requests${queryForTenant(tenant, 'limit=5')}`, token),
      ]);
      return { upstreams, requests };
    },
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
  if (resource.state.kind === 'idle' || resource.state.kind === 'loading') return <div className="empty">{t('common.loading')}</div>;
  if (resource.state.kind === 'failed') return <div className="notice error" role="alert">{resource.state.message}</div>;
  return <div className="operator-overview-dashboard">
    {resource.state.refreshError && <div className="notice error" role="alert">{resource.state.refreshError}</div>}
    <article className="panel operator-overview-shortcuts"><div className="panel-title"><div><h2>{t('usage.overview')}</h2><p className="muted">{t('operator.subtitle')}</p></div></div><div className="row-actions">{destinations.map((item) => <button type="button" className="secondary" key={item.route} onClick={() => onNavigate(item.route)}>{item.label}</button>)}</div></article>
    <article className="panel operator-overview-recent"><div className="panel-title"><h2>{t('self.recent')}</h2><span>{t('sessions.requests')}</span></div><RequestTable requests={resource.state.value.requests} onOpenSession={onOpenSession} /></article>
    <UsageAnalysis token={token} tenant={tenant} upstreams={resource.state.value.upstreams} onOpenSession={onOpenUsageSession} />
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
  if (resource.state.kind === 'idle' || resource.state.kind === 'loading') return <div className="empty">{t('common.loading')}</div>;
  if (resource.state.kind === 'failed') return <div className="notice error" role="alert">{resource.state.message}</div>;
  return <>{resource.state.refreshError && <div className="notice error" role="alert">{resource.state.refreshError}</div>}<UsageAnalysis token={token} tenant={tenant} upstreams={resource.state.value} onOpenSession={onOpenSession} /></>;
}

export function GenerationsPage({ token, tenant }: OperatorPageProps) {
  return <GenerationWorkspace token={token} tenant={tenant} />;
}

export function PluginsPage({ token, tenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<PluginManifest[]>('/internal/v1/plugins', token),
    t('common.requestFailed'),
  );
  if (resource.state.kind === 'idle' || resource.state.kind === 'loading') return <div className="empty">{t('common.loading')}</div>;
  if (resource.state.kind === 'failed') return <div className="notice error" role="alert">{resource.state.message}</div>;
  return <>{resource.state.refreshError && <div className="notice error" role="alert">{resource.state.refreshError}</div>}<Plugins token={token} tenant={tenant} values={resource.state.value} /></>;
}
