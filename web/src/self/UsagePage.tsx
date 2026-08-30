import { lazy, Suspense, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { api } from '../api';
import { HeatmapDataTable } from '../charts/HeatmapDataTable';
import { costOption, heatmapOption, latencyOption, throughputOption, totalTokens, type UsageChartCopy, type UsageChartFormatters } from '../charts/usageCharts';
import { Metric, NumberMetric } from '../components';
import { formatCurrency, formatMilliseconds, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import type { KeyView, SelfUsageAnalysis, UsageAnalysisBucket, UsageAnalysisCost, UsageAnalysisTimeBucket } from '../types';
import '../operator/usage.css';
import { selfErrorMessage } from './errors';

const EChart = lazy(() => import('../charts/EChart').then((module) => ({ default: module.EChart })));
type UsageRange = '24h' | '7d' | '30d';

function rangeMillis(range: UsageRange) {
  if (range === '7d') return 7 * 86_400_000;
  if (range === '30d') return 30 * 86_400_000;
  return 86_400_000;
}

function CostLines({ values }: { values: UsageAnalysisCost[] }) {
  const { locale } = useI18n();
  if (!values.length) return <>—</>;
  return <span className="usage-cost-lines">{[...values]
    .sort((left, right) => left.currency.localeCompare(right.currency))
    .map((value) => <span key={value.currency}>{formatCurrency(value.cost, value.currency, locale)}</span>)}</span>;
}

function ChartDataTable({ timeZone, values }: { timeZone: string; values: UsageAnalysisTimeBucket[] }) {
  const { locale, t } = useI18n();
  return <details className="usage-chart-table"><summary>{t('usage.trendData')}</summary><div className="table-scroll"><table>
    <thead><tr><th>{t('request.time')} · {timeZone}</th><th>{t('traffic.success')}</th><th>{t('traffic.failure')}</th><th>{t('usage.totalTokens')}</th><th>{t('usage.average')}</th><th>{t('usage.p95Approx')}</th><th>{t('usage.cost')}</th></tr></thead>
    <tbody>{values.map((value) => <tr key={value.bucket_start}><td>{new Date(value.bucket_start).toLocaleString(locale, { timeZone })}</td><td>{formatNumber(value.success, locale)}</td><td>{formatNumber(value.failed, locale)}</td><td>{formatNumber(totalTokens(value), locale)}</td><td>{formatMilliseconds(value.avg_duration_ms, locale)}</td><td>{formatMilliseconds(value.p95_duration_ms, locale)}</td><td><CostLines values={value.costs} /></td></tr>)}</tbody>
  </table></div></details>;
}

function ChartPanel({ children, timeZone, title, values }: { children: ReactNode; timeZone: string; title: string; values: UsageAnalysisTimeBucket[] }) {
  const { t } = useI18n();
  return <article className="panel usage-chart-card"><div className="panel-title"><h2>{title}</h2><span>{timeZone}</span></div>{values.length === 0 ? <div className="empty">{t('usage.noTrendData')}</div> : <>{children}<ChartDataTable timeZone={timeZone} values={values} /></>}</article>;
}

function DimensionTable({ title, values }: { title: string; values: UsageAnalysisBucket[] }) {
  const { locale, t } = useI18n();
  return <article className="panel usage-dimension"><div className="panel-title"><h2>{title}</h2><span>{formatNumber(values.length, locale)}</span></div>
    {values.length === 0 ? <div className="empty">{t('usage.noDimensionData')}</div> : <div className="table-scroll"><table><thead><tr><th>{title}</th><th>{t('usage.requests')}</th><th>{t('usage.successRate')}</th><th>{t('usage.totalTokens')}</th><th>{t('usage.cost')}</th></tr></thead><tbody>
      {values.map((value) => <tr key={value.id}><td>{value.label}</td><td>{formatNumber(value.requests, locale)}</td><td>{formatPercent(value.requests ? value.success / value.requests : null, locale)}</td><td>{formatNumber(totalTokens(value), locale)}</td><td><CostLines values={value.costs} /></td></tr>)}
    </tbody></table></div>}
  </article>;
}

export function UsagePage({ credential, credentialView, onError }: {
  credential: string;
  credentialView: KeyView;
  onError: (message: string) => void;
}) {
  const { locale, t } = useI18n();
  const [range, setRange] = useState<UsageRange>('24h');
  const [refresh, setRefresh] = useState(0);
  const scope = useMemo(() => ({}), [credential, range, refresh]);
  const [remote, setRemote] = useState<{ scope: object; status: 'loading' } | { scope: object; status: 'ready'; value: SelfUsageAnalysis } | { scope: object; status: 'error'; message: string }>();
  const sequence = useRef(0);

  useEffect(() => {
    const current = ++sequence.current;
    const to = Date.now();
    const query = new URLSearchParams({ from_created_at: String(to - rangeMillis(range)), to_created_at: String(to), granularity: 'auto' });
    setRemote({ scope, status: 'loading' });
    onError('');
    void api<SelfUsageAnalysis>(`/self/v1/usage-analysis?${query}`, credential).then((response) => {
      if (current === sequence.current) setRemote({ scope, status: 'ready', value: response });
    }).catch((reason) => {
      if (current === sequence.current) {
        const message = selfErrorMessage(reason, t, t('common.requestFailed'));
        setRemote({ scope, status: 'error', message });
        onError(message);
      }
    });
    return () => { sequence.current += 1; };
  }, [credential, range, refresh, scope]);

  const scopedRemote = remote?.scope === scope ? remote : { scope, status: 'loading' as const };
  const stats = scopedRemote.status === 'ready' ? scopedRemote.value : undefined;
  const timeZone = stats?.time_zone ?? 'UTC';

  const copy: UsageChartCopy = useMemo(() => ({
    requests: t('usage.requests'), success: t('traffic.success'), failures: t('traffic.failure'), averageLatency: t('usage.average'),
    p95Latency: t('usage.p95Approx'), cost: t('usage.cost'), noData: t('usage.noTrendData'),
  }), [t]);
  const formatters: UsageChartFormatters = useMemo(() => ({
    bucket: (epoch) => new Date(epoch).toLocaleString(locale, { timeZone }),
    cost: (value, currency) => formatCurrency(value, currency, locale), duration: (value) => formatMilliseconds(value, locale),
    number: (value) => formatNumber(value, locale), percent: (value) => formatPercent(value, locale),
  }), [locale, timeZone]);
  const throughput = useMemo(() => throughputOption(stats?.time_series ?? [], copy, formatters), [stats?.time_series, copy, formatters]);
  const latency = useMemo(() => latencyOption(stats?.time_series ?? [], copy, formatters), [stats?.time_series, copy, formatters]);
  const costs = useMemo(() => costOption(stats?.time_series ?? [], copy, formatters), [stats?.time_series, copy, formatters]);
  const weekdays = useMemo(() => Array.from({ length: 7 }, (_, day) => new Date(Date.UTC(2024, 0, 8 + day)).toLocaleDateString(locale, { weekday: 'short', timeZone })), [locale, timeZone]);
  const heatmap = useMemo(() => heatmapOption(stats?.heatmap ?? [], 'requests', credentialView.currency, weekdays, t('usage.heatmapLabel'), formatters), [stats?.heatmap, credentialView.currency, weekdays, t, formatters]);

  if (scopedRemote.status === 'loading') return <div className="self-page usage-page"><div className="usage-heading"><div><h2>{t('usage.title')}</h2><p className="muted">{t('self.usageDescription')}</p></div></div><div className="boot" role="status">{t('common.loading')}</div></div>;
  if (scopedRemote.status === 'error') return <div className="self-page usage-page"><div className="usage-heading"><div><h2>{t('usage.title')}</h2><p className="muted">{t('self.usageDescription')}</p></div><button type="button" className="secondary" onClick={() => setRefresh((value) => value + 1)}>{t('usage.refresh')}</button></div><div className="notice error" role="alert">{scopedRemote.message}</div></div>;
  if (!stats) return <div className="empty">{t('common.noData')}</div>;
  const successRate = stats.summary.requests ? stats.summary.success / stats.summary.requests : null;
  return <div className="self-page self-usage-page usage-page" data-self-page="usage">
    <div className="usage-heading"><div><h2>{t('usage.title')}</h2><p className="muted">{t('self.usageDescription')}</p><span className="usage-time-zone">{timeZone}</span></div><div className="usage-presets" role="group" aria-label={t('usage.timeRange')}>{(['24h', '7d', '30d'] as UsageRange[]).map((value) => <button type="button" key={value} className={range === value ? 'active' : 'secondary'} aria-pressed={range === value} onClick={() => setRange(value)}>{t(`usage.preset.${value}`)}</button>)}</div></div>
    <section className="metrics self-usage-metrics">
      <NumberMetric label={t('usage.requests')} value={stats.summary.requests} />
      <Metric label={t('usage.successRate')} value={formatPercent(successRate, locale)} tone="positive" />
      <NumberMetric label={t('usage.failures')} value={stats.summary.failed} tone="negative" />
      <NumberMetric label={t('usage.totalTokens')} value={totalTokens(stats.summary)} showCompact={false} />
      <NumberMetric label={t('usage.cachedTokens')} value={stats.summary.cached_input_tokens} showCompact={false} />
      <NumberMetric label={t('usage.cacheWriteTokens')} value={stats.summary.cache_write_tokens} showCompact={false} />
      <Metric label={t('usage.average')} value={formatMilliseconds(stats.summary.avg_duration_ms, locale)} />
      <Metric label={t('usage.p95Approx')} value={formatMilliseconds(stats.summary.p95_duration_ms, locale)} />
      <Metric label={t('usage.cost')} value={<CostLines values={stats.summary.costs} />} />
    </section>
    <Suspense fallback={<div className="empty">{t('common.loading')}</div>}>
      <section className="usage-chart-grid self-usage-charts">
        <ChartPanel timeZone={timeZone} title={t('usage.throughput')} values={stats.time_series}><EChart ariaLabel={t('usage.throughput')} locale={locale} option={throughput} timeZone={timeZone} /></ChartPanel>
        <ChartPanel timeZone={timeZone} title={t('usage.latencyTrend')} values={stats.time_series}><EChart ariaLabel={t('usage.latencyTrend')} locale={locale} option={latency} timeZone={timeZone} /></ChartPanel>
        <ChartPanel timeZone={timeZone} title={t('usage.costTrend')} values={stats.time_series}><EChart ariaLabel={t('usage.costTrend')} locale={locale} option={costs} timeZone={timeZone} /></ChartPanel>
        <article className="panel usage-chart-card usage-heatmap-panel"><div className="panel-title"><h2>{t('usage.heatmap')}</h2><span>{timeZone}</span></div>{stats.heatmap.length === 0 ? <div className="empty">{t('usage.noHeatmapData')}</div> : <><EChart ariaLabel={t('usage.heatmapLabel')} className="usage-echart-heatmap" locale={locale} option={heatmap} timeZone={timeZone} /><HeatmapDataTable currency={credentialView.currency} format={formatters} metric="requests" summary={t('usage.trendData')} timeZone={timeZone} valueLabel={t('usage.requests')} values={stats.heatmap} weekdays={weekdays} /></>}</article>
      </section>
    </Suspense>
    <section className="two-column self-usage-breakdown"><DimensionTable title={t('usage.models')} values={stats.by_model} /><DimensionTable title={t('usage.protocols')} values={stats.by_protocol} /></section>
    <section className="two-column self-usage-breakdown"><DimensionTable title={t('usage.statuses')} values={stats.by_status} /><DimensionTable title={t('usage.errors')} values={stats.errors} /></section>
  </div>;
}
