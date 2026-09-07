import { api } from '../../api';
import { Metric, RequestTable } from '../../components';
import { formatCurrency, formatMetricNumber, formatNumber, formatPercent } from '../../format';
import { useI18n } from '../../i18n';
import type { OperatorStats, PluginManifest, RequestView, UpstreamAccount, UsageAnalysisSessionBucket } from '../../types';
import { GenerationWorkspace } from '../GenerationWorkspace';
import { Plugins } from '../Plugins';
import { UsageAnalysis } from '../UsageAnalysis';
import { useOperatorResource } from '../hooks/useOperatorResource';
import type { OperatorRouteKey } from '../scope/operatorRoutes';
import { queryForTenant } from '../scope/operatorShared';

interface OperatorPageProps {
  token: string;
  /** Selected read scope; empty only when no tenant is available. */
  tenant: string;
  /** Explicit target for any mutation rendered by the page. */
  writeTenant?: string;
}

function overviewStatsPath(tenant: string) {
  const query = new URLSearchParams({
    from_created_at: String(Date.now() - 86_400_000),
    to_created_at: String(Date.now()),
  });
  if (tenant) query.set('tenant_external_id', tenant);
  return `/internal/v1/stats?${query}`;
}

function OverviewCost({ costs }: { costs: OperatorStats['summary']['costs'] }) {
  const { locale } = useI18n();
  if (!costs.length) return <>—</>;
  return <span className="usage-cost-lines">
    {costs.map(({ currency, cost }) => <span key={currency} title={`${cost} ${currency}`}>
      {formatCurrency(cost, currency, locale)}
    </span>)}
  </span>;
}

function OverviewMetrics({ stats }: { stats: OperatorStats }) {
  const { locale, t } = useI18n();
  const summary = stats.summary;
  const requests = formatMetricNumber(summary.total_requests, locale);
  const tokens = formatMetricNumber(summary.input_tokens + summary.output_tokens, locale);
  const successRate = summary.total_requests > 0
    ? summary.successful_requests / summary.total_requests
    : undefined;
  return <section className="metrics operator-overview-metrics" aria-label={t('usage.overview')}>
    <Metric label={t('usage.requests')} value={<span title={requests.title}>{requests.text}</span>} />
    <Metric label={t('usage.successRate')} value={formatPercent(successRate, locale)} tone="positive" />
    <Metric label={t('usage.failures')} value={formatNumber(summary.failed_requests, locale)} tone="negative" />
    <Metric label={t('usage.totalTokens')} value={<span title={tokens.title}>{tokens.text}</span>} />
    <Metric label={t('usage.cost')} value={<OverviewCost costs={summary.costs} />} />
  </section>;
}

export function OverviewPage({ token, tenant, onNavigate, onOpenSession }: OperatorPageProps & {
  onNavigate: (route: OperatorRouteKey) => void;
  onOpenUsageSession: (session: UsageAnalysisSessionBucket) => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    async () => {
      const [requests, stats] = await Promise.all([
        api<RequestView[]>(`/internal/v1/requests${queryForTenant(tenant, 'limit=5')}`, token),
        api<OperatorStats>(overviewStatsPath(tenant), token),
      ]);
      return { requests, stats };
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
    <OverviewMetrics stats={resource.state.value.stats} />
    <article className="panel operator-overview-recent"><div className="panel-title"><h2>{t('self.recent')}</h2><span>{t('sessions.requests')}</span></div><RequestTable requests={resource.state.value.requests} onOpenSession={onOpenSession} /></article>
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
