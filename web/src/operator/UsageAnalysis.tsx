import { lazy, Suspense, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { api } from '../api';
import { HeatmapDataTable } from '../charts/HeatmapDataTable';
import {
  costCurrencies, costOption, heatmapOption, heatmapValue, latencyOption,
  throughputOption, totalTokens, type HeatmapMetric, type UsageChartCopy, type UsageChartFormatters,
} from '../charts/usageCharts';
import { Metric } from '../components';
import { formatCurrency, formatMetricNumber, formatMilliseconds, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import type { OperatorUsageAnalysis, UsageAnalysisBucket, UsageAnalysisCost, UsageAnalysisMetrics, UsageAnalysisSessionBucket, UsageAnalysisTimeBucket, UpstreamAccount } from '../types';
import './usage.css';
import { localDateTimeInput, nextUsageTab, statsQuery, usageTabs, type Granularity, type Preset, type UsageSelection, type UsageTab } from './usageState';

const EChart = lazy(() => import('../charts/EChart').then((module) => ({ default: module.EChart })));

type Dimension = 'models' | 'keys' | 'sessions' | 'upstreams' | 'protocols' | 'statuses' | 'errors';
interface UsageFilters { model: string; keyId: string; upstreamId: string; protocol: string; status: string; errorCode: string }
const dimensions: Dimension[] = ['models', 'keys', 'sessions', 'upstreams', 'protocols', 'statuses', 'errors'];
const presets: Preset[] = ['24h', 'today', 'yesterday', '7d', '30d', 'custom'];
const emptyFilters: UsageFilters = { model: '', keyId: '', upstreamId: '', protocol: '', status: '', errorCode: '' };
const localCopy = {
  'zh-CN': {
    dimensions: '维度分析', filters: '筛选条件', charts: '图表数据', throughput: '请求吞吐', latency: '响应延迟', costTrend: '费用趋势',
    averageLatency: '平均延迟', p95Latency: 'P95 延迟（近似）', heatMetric: '热力指标', requests: '请求数', success: '成功', failures: '失败',
    tokens: 'Token', cost: '费用', failureRate: '失败率', selectedCell: '已选择', chartEmpty: '当前范围没有可绘制的数据', filtersActive: '项筛选已生效', category: '分类',
  },
  en: {
    dimensions: 'Dimensions', filters: 'Filters', charts: 'Chart data', throughput: 'Request throughput', latency: 'Response latency', costTrend: 'Cost trend',
    averageLatency: 'Average latency', p95Latency: 'P95 latency (approx.)', heatMetric: 'Heat metric', requests: 'Requests', success: 'Successful', failures: 'Failed',
    tokens: 'Tokens', cost: 'Cost', failureRate: 'Failure rate', selectedCell: 'Selected', chartEmpty: 'No chartable data in this range', filtersActive: 'active filters', category: 'Category',
  },
} as const;

function NumericMetric({ label, value, tone }: { label: string; value?: number | null; tone?: string }) {
  const { locale } = useI18n(); const formatted = formatMetricNumber(value, locale);
  return <Metric label={label} tone={tone} value={<span title={formatted.title}>{formatted.text}</span>} />;
}
function CostValue({ costs }: { costs: UsageAnalysisCost[] }) {
  const { locale } = useI18n(); if (!costs.length) return <span>—</span>;
  return <span className="usage-cost-lines">{[...costs].sort((a, b) => a.currency.localeCompare(b.currency)).map(({ currency, cost }) => <span key={currency} title={`${cost} ${currency}`}>{formatCurrency(cost, currency, locale)}</span>)}</span>;
}
function successRate(metrics: UsageAnalysisMetrics) { return metrics.requests > 0 ? metrics.success / metrics.requests : undefined; }

function GenerationBreakdown({ stats }: { stats: OperatorUsageAnalysis }) {
  const { locale, t } = useI18n(); const copy = localCopy[locale];
  const rows = [
    ...(stats.generation_units_by_modality ?? []).map((value) => ({ id: `modality:${value.modality}:${value.currency}`, group: t('usage.generationByModality'), name: t(`modality.${value.modality}`), currency: value.currency, units: value.units })),
    ...(stats.generation_units_by_billing_unit ?? []).map((value) => ({ id: `unit:${value.billing_unit}:${value.currency}`, group: t('usage.generationByBillingUnit'), name: t(`billingUnit.${value.billing_unit}`), currency: value.currency, units: value.units })),
  ];
  if (!rows.length) return null;
  return <article className="panel usage-generation-table"><div className="table-scroll"><table><thead><tr><th>{t('usage.generationBreakdown')}</th><th>{copy.category}</th><th>{t('usage.generationUnits')}</th></tr></thead><tbody>{rows.map((row) => <tr key={row.id}><td>{row.group}</td><td>{row.name} · {row.currency}</td><td>{formatNumber(row.units, locale)}</td></tr>)}</tbody></table></div></article>;
}

function DimensionTable<T extends UsageAnalysisBucket>({ title, values, onSelect, labelForValue }: { title: string; values: T[]; onSelect?: (value: T) => void; labelForValue?: (value: T) => string }) {
  const { locale, t } = useI18n();
  return <article className="panel usage-dimension"><div className="panel-title"><h2>{title}</h2><span>{formatNumber(values.length, locale)}</span></div>
    {values.length === 0 ? <div className="empty">{t('usage.noDimensionData')}</div> : <div className="table-scroll"><table><thead><tr><th>{title}</th><th>{t('usage.requests')}</th><th>{t('usage.tokens')}</th><th>{t('usage.generationUnits')}</th><th>{t('usage.cost')}</th><th>{t('usage.successRate')}</th></tr></thead><tbody>{values.map((value) => {
      const label = labelForValue?.(value) || value.label || t('common.none'); const requests = formatMetricNumber(value.requests, locale); const tokens = formatMetricNumber(totalTokens(value), locale); const units = formatMetricNumber(value.generation_units, locale);
      return <tr key={value.id}><td>{onSelect ? <button type="button" className="table-link usage-filter-link" onClick={() => onSelect(value)}>{label}</button> : label}</td><td title={requests.title}>{requests.text}</td><td title={tokens.title}>{tokens.text}</td><td title={units.title}>{units.text}</td><td><CostValue costs={value.costs} /></td><td>{formatPercent(successRate(value), locale)}</td></tr>;
    })}</tbody></table></div>}
  </article>;
}

function ChartTable({ onSelect, points, timeZone }: { onSelect?: (point: UsageAnalysisTimeBucket) => void; points: UsageAnalysisTimeBucket[]; timeZone: string }) {
  const { locale, t } = useI18n(); const copy = localCopy[locale];
  return <details className="usage-chart-table"><summary>{copy.charts}</summary><div className="table-scroll"><table><thead><tr><th>{t('traffic.from')} · {timeZone}</th><th>{copy.success}</th><th>{copy.failures}</th><th>{copy.tokens}</th><th>{copy.averageLatency}</th><th>{copy.p95Latency}</th><th>{copy.cost}</th></tr></thead><tbody>{points.map((point) => { const label = new Date(point.bucket_start).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN', { timeZone }); return <tr key={point.bucket_start}><td>{onSelect ? <button type="button" className="table-link" onClick={() => onSelect(point)}>{label}</button> : label}</td><td>{formatNumber(point.success, locale)}</td><td>{formatNumber(point.failed, locale)}</td><td>{formatNumber(totalTokens(point), locale)}</td><td>{formatMilliseconds(point.avg_duration_ms, locale)}</td><td>{formatMilliseconds(point.p95_duration_ms, locale)}</td><td><CostValue costs={point.costs} /></td></tr>; })}</tbody></table></div></details>;
}
function ChartCard({ title, children, onSelect, points, timeZone }: { title: string; children: ReactNode; onSelect?: (point: UsageAnalysisTimeBucket) => void; points: UsageAnalysisTimeBucket[]; timeZone: string }) {
  const { locale } = useI18n();
  return <article className="panel usage-chart-card"><div className="panel-title"><h2>{title}</h2><span>{timeZone}</span></div>{points.length === 0 ? <div className="empty">{localCopy[locale].chartEmpty}</div> : <>{children}<ChartTable onSelect={onSelect} points={points} timeZone={timeZone} /></>}</article>;
}

export function UsageAnalysis({ token, tenant, upstreams, onOpenSession }: { token: string; tenant: string; upstreams: UpstreamAccount[]; onOpenSession: (session: UsageAnalysisSessionBucket) => void }) {
  const { locale, t } = useI18n(); const copy = localCopy[locale]; const now = Date.now();
  const [tab, setTab] = useState<UsageTab>('overview'); const [dimension, setDimension] = useState<Dimension>('models');
  const [heatMetric, setHeatMetric] = useState<HeatmapMetric>('requests'); const [heatCurrency, setHeatCurrency] = useState(''); const [selectedHeatHour, setSelectedHeatHour] = useState<number>();
  const [selection, setSelection] = useState<UsageSelection>({ preset: '24h', granularity: 'auto', customFrom: localDateTimeInput(now - 86_400_000), customTo: localDateTimeInput(now), filters: emptyFilters });
  const [applied, setApplied] = useState(selection); const [refresh, setRefresh] = useState(0); const scope = useMemo(() => ({}), [token, tenant, applied, refresh]);
  const [remote, setRemote] = useState<{ scope: object; status: 'loading' } | { scope: object; status: 'ready'; value: OperatorUsageAnalysis } | { scope: object; status: 'error'; message: string }>(); const requestSequence = useRef(0);

  useEffect(() => {
    const sequence = ++requestSequence.current;
    if (!token.trim()) { setRemote(undefined); return; }
    const query = statsQuery(tenant, applied); if (!query) { setRemote({ scope, status: 'error', message: t('usage.invalidRange') }); return; }
    setRemote({ scope, status: 'loading' });
    void api<OperatorUsageAnalysis>(`/internal/v1/usage-analysis${query}`, token.trim()).then((value) => { if (sequence === requestSequence.current) setRemote({ scope, status: 'ready', value }); }).catch((reason: unknown) => { if (sequence === requestSequence.current) setRemote({ scope, status: 'error', message: reason instanceof Error ? reason.message : t('usage.loadFailed') }); });
  }, [token, tenant, applied, refresh, scope, t]);

  const scopedRemote = remote?.scope === scope ? remote : token.trim() ? { scope, status: 'loading' as const } : undefined;
  const stats = scopedRemote?.status === 'ready' ? scopedRemote.value : undefined;
  const loading = scopedRemote?.status === 'loading';
  const error = scopedRemote?.status === 'error' ? scopedRemote.message : '';

  const applyDimension = (filter: keyof UsageFilters, bucket: UsageAnalysisBucket) => { const next = { ...selection, filters: { ...selection.filters, [filter]: bucket.id } }; setSelection(next); setApplied(next); };
  const selectUtcBucket = (point: UsageAnalysisTimeBucket) => { const millis = stats?.granularity === 'hour' ? 3_600_000 : 86_400_000; const next = { ...selection, preset: 'custom' as const, customFrom: localDateTimeInput(point.bucket_start), customTo: localDateTimeInput(point.bucket_start + millis - 1) }; setSelection(next); setApplied(next); setTab('overview'); };

  const chartCopy: UsageChartCopy = useMemo(() => ({ requests: copy.requests, success: copy.success, failures: copy.failures, averageLatency: copy.averageLatency, p95Latency: copy.p95Latency, cost: copy.cost, noData: copy.chartEmpty }), [copy]);
  const chartFormatters: UsageChartFormatters = useMemo(() => ({ bucket: (epoch) => new Date(epoch).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN', { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false, timeZone: stats?.time_zone ?? 'UTC' }), cost: (value, currency) => formatCurrency(value, currency, locale), duration: (value) => formatMilliseconds(value, locale), number: (value) => formatNumber(value, locale), percent: (value) => formatPercent(value, locale) }), [locale, stats?.time_zone]);
  const throughput = useMemo(() => throughputOption(stats?.time_series ?? [], chartCopy, chartFormatters), [stats?.time_series, chartCopy, chartFormatters]);
  const latency = useMemo(() => latencyOption(stats?.time_series ?? [], chartCopy, chartFormatters), [stats?.time_series, chartCopy, chartFormatters]);
  const costs = useMemo(() => costOption(stats?.time_series ?? [], chartCopy, chartFormatters), [stats?.time_series, chartCopy, chartFormatters]);
  const currencies = stats ? costCurrencies(stats.heatmap) : []; const effectiveHeatCurrency = currencies.includes(heatCurrency) ? heatCurrency : (currencies[0] ?? 'USD');
  const weekdays = useMemo(() => Array.from({ length: 7 }, (_, day) => new Date(Date.UTC(2024, 0, 8 + day)).toLocaleDateString(locale === 'en' ? 'en-US' : 'zh-CN', { weekday: 'short', timeZone: stats?.time_zone ?? 'UTC' })), [locale, stats?.time_zone]);
  const heatmap = useMemo(() => heatmapOption(stats?.heatmap ?? [], heatMetric, effectiveHeatCurrency, weekdays, t('usage.heatmapLabel'), chartFormatters), [stats?.heatmap, heatMetric, effectiveHeatCurrency, weekdays, t, chartFormatters]);
  const activeFilters = Object.values(applied.filters).filter(Boolean).length;
  const heatMetricLabel = heatMetric === 'requests' ? copy.requests : heatMetric === 'tokens' ? copy.tokens : heatMetric === 'cost' ? copy.cost : copy.failureRate;
  const selectedHeatCell = stats?.heatmap.find((value) => value.hour_of_week === selectedHeatHour);

  const renderDimension = () => {
    if (!stats) return null;
    if (dimension === 'models') return <DimensionTable title={t('usage.models')} values={stats.by_model} onSelect={(bucket) => applyDimension('model', bucket)} />;
    if (dimension === 'keys') return <DimensionTable title={t('usage.keys')} values={stats.by_key} onSelect={(bucket) => applyDimension('keyId', bucket)} />;
    if (dimension === 'sessions') return <DimensionTable title={t('usage.sessions')} values={stats.by_session} labelForValue={(bucket) => bucket.unlinked || bucket.id.startsWith('unlinked:') ? t('sessions.unlinkedRequests') : bucket.label} onSelect={onOpenSession} />;
    if (dimension === 'upstreams') return <DimensionTable title={t('usage.upstreams')} values={stats.by_upstream} labelForValue={(bucket) => bucket.id === 'unassigned' ? t('usage.unassigned') : bucket.label} onSelect={(bucket) => applyDimension('upstreamId', bucket)} />;
    if (dimension === 'protocols') return <DimensionTable title={t('usage.protocols')} values={stats.by_protocol} labelForValue={(bucket) => t(`usage.protocol.${bucket.id}`)} onSelect={(bucket) => applyDimension('protocol', bucket)} />;
    if (dimension === 'statuses') return <DimensionTable title={t('usage.statuses')} values={stats.by_status} labelForValue={(bucket) => bucket.id === 'success' ? t('traffic.success') : bucket.id === 'error' ? t('traffic.failure') : bucket.label} onSelect={(bucket) => applyDimension('status', bucket)} />;
    return <DimensionTable title={t('usage.errors')} values={stats.errors} onSelect={(bucket) => applyDimension('errorCode', bucket)} />;
  };

  return <div className="usage-page"><div className="usage-heading"><div><h2>{t('usage.title')}</h2><p className="muted">{t('usage.description')}</p>{stats && <span className="usage-time-zone">{stats.time_zone}</span>}</div><button type="button" className="secondary" disabled={loading || !token.trim()} onClick={() => setRefresh((value) => value + 1)}>{loading ? t('common.loading') : t('usage.refresh')}</button></div>
    <details className="usage-filter-disclosure"><summary>{copy.filters}{activeFilters > 0 && <span className="usage-filter-count">{activeFilters} {copy.filtersActive}</span>}</summary><form className="usage-controls" onSubmit={(event) => { event.preventDefault(); setApplied({ ...selection }); }}>
      <fieldset><legend>{t('usage.timeRange')}</legend><div className="usage-presets">{presets.map((preset) => <button type="button" className={selection.preset === preset ? 'active' : 'secondary'} aria-pressed={selection.preset === preset} key={preset} onClick={() => { const next = { ...selection, preset }; setSelection(next); if (preset !== 'custom') setApplied(next); }}>{t(`usage.preset.${preset}`)}</button>)}</div></fieldset>
      {selection.preset === 'custom' && <div className="usage-custom-range"><label>{t('traffic.from')}<input type="datetime-local" step="0.001" value={selection.customFrom} onChange={(event) => setSelection({ ...selection, customFrom: event.target.value })} /></label><label>{t('traffic.to')}<input type="datetime-local" step="0.001" value={selection.customTo} onChange={(event) => setSelection({ ...selection, customTo: event.target.value })} /></label></div>}
      <div className="usage-filter-grid"><label>{t('usage.granularity')}<select value={selection.granularity} onChange={(event) => setSelection({ ...selection, granularity: event.target.value as Granularity })}><option value="auto">{t('usage.granularity.auto')}</option><option value="hour">{t('usage.granularity.hour')}</option><option value="day">{t('usage.granularity.day')}</option></select></label><label>{t('request.model')}<input value={selection.filters.model} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, model: event.target.value } })} /></label><label>{t('traffic.keyId')}<input value={selection.filters.keyId} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, keyId: event.target.value } })} placeholder="019f…" /></label><label>{t('traffic.upstream')}<select value={selection.filters.upstreamId} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, upstreamId: event.target.value } })}><option value="">{t('common.all')}</option><option value="unassigned">{t('usage.unassigned')}</option>{upstreams.map((value) => <option value={value.id} key={value.id}>{value.name}</option>)}</select></label><label>{t('request.protocol')}<select value={selection.filters.protocol} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, protocol: event.target.value } })}><option value="">{t('common.all')}</option>{['openai', 'anthropic', 'openai-image', 'generation'].map((protocol) => <option value={protocol} key={protocol}>{t(`usage.protocol.${protocol}`)}</option>)}</select></label><label>{t('request.status')}<select value={selection.filters.status} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, status: event.target.value } })}><option value="">{t('common.all')}</option><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option></select></label><label>{t('traffic.errorCode')}<input value={selection.filters.errorCode} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, errorCode: event.target.value } })} /></label><div className="filter-actions"><button type="submit" disabled={loading || !token.trim()}>{t('usage.apply')}</button><button type="button" className="secondary" disabled={loading || !Object.values(selection.filters).some(Boolean)} onClick={() => { const next = { ...selection, filters: emptyFilters }; setSelection(next); setApplied(next); }}>{t('usage.clearFilters')}</button></div></div>
    </form></details>
    {!token.trim() && <div className="notice warning" role="status">{t('usage.connectPrompt')}</div>}{loading && <div className="notice" role="status">{t('common.loading')}</div>}{error && <div className="notice error" role="alert">{error}</div>}
    <nav className="usage-tabs" role="tablist" aria-label={t('usage.sections')}>{usageTabs.map((id) => <button type="button" role="tab" id={`usage-tab-${id}`} aria-controls={`usage-panel-${id}`} aria-selected={tab === id} tabIndex={tab === id ? 0 : -1} className={tab === id ? 'active' : ''} key={id} onClick={() => setTab(id)} onKeyDown={(event) => { const next = nextUsageTab(id, event.key); if (!next) return; event.preventDefault(); setTab(next); requestAnimationFrame(() => document.getElementById(`usage-tab-${next}`)?.focus()); }}>{id === 'dimensions' ? copy.dimensions : t(`usage.tab.${id}`)}</button>)}</nav>
    {stats && <Suspense fallback={<div className="empty">{t('common.loading')}</div>}><section className="usage-tab-panel" role="tabpanel" id={`usage-panel-${tab}`} aria-labelledby={`usage-tab-${tab}`}>{tab === 'overview' && <><section className="metrics usage-metrics"><NumericMetric label={t('usage.requests')} value={stats.summary.requests} /><Metric label={t('usage.successRate')} value={formatPercent(successRate(stats.summary), locale)} tone="positive" /><NumericMetric label={t('usage.failures')} value={stats.summary.failed} tone="negative" /><Metric label={t('usage.cost')} value={<CostValue costs={stats.summary.costs} />} /><NumericMetric label={t('usage.totalTokens')} value={totalTokens(stats.summary)} /><NumericMetric label={t('usage.generationUnits')} value={stats.summary.generation_units} /><NumericMetric label={t('usage.cachedTokens')} value={stats.summary.cached_input_tokens} /><Metric label={copy.p95Latency} value={formatMilliseconds(stats.summary.p95_duration_ms, locale)} /></section><GenerationBreakdown stats={stats} /><ChartCard title={copy.throughput} onSelect={selectUtcBucket} points={stats.time_series} timeZone={stats.time_zone}><EChart ariaLabel={copy.throughput} locale={locale} option={throughput} timeZone={stats.time_zone} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></ChartCard></>}
      {tab === 'trend' && <div className="usage-chart-grid"><ChartCard title={copy.throughput} onSelect={selectUtcBucket} points={stats.time_series} timeZone={stats.time_zone}><EChart ariaLabel={copy.throughput} locale={locale} option={throughput} timeZone={stats.time_zone} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></ChartCard><ChartCard title={copy.latency} onSelect={selectUtcBucket} points={stats.time_series} timeZone={stats.time_zone}><EChart ariaLabel={copy.latency} locale={locale} option={latency} timeZone={stats.time_zone} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></ChartCard><ChartCard title={copy.costTrend} onSelect={selectUtcBucket} points={stats.time_series} timeZone={stats.time_zone}><EChart ariaLabel={copy.costTrend} locale={locale} option={costs} timeZone={stats.time_zone} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></ChartCard></div>}
      {tab === 'dimensions' && <><div className="usage-dimension-picker" role="tablist" aria-label={copy.dimensions}>{dimensions.map((id) => <button key={id} type="button" className={dimension === id ? 'active' : 'secondary'} aria-pressed={dimension === id} onClick={() => setDimension(id)}>{t(`usage.${id}`)}</button>)}</div>{renderDimension()}</>}
      {tab === 'heatmap' && <article className="panel usage-heatmap-panel"><div className="panel-title usage-heatmap-title"><h2>{t('usage.heatmap')}</h2><div><span>{stats.time_zone}</span><label>{copy.heatMetric}<select value={heatMetric} onChange={(event) => { setHeatMetric(event.target.value as HeatmapMetric); setSelectedHeatHour(undefined); }}><option value="requests">{copy.requests}</option><option value="tokens">{copy.tokens}</option><option value="cost">{copy.cost}</option><option value="failure_rate">{copy.failureRate}</option></select></label>{heatMetric === 'cost' && <label>{t('usage.trendCurrency')}<select value={effectiveHeatCurrency} onChange={(event) => { setHeatCurrency(event.target.value); setSelectedHeatHour(undefined); }}>{currencies.map((currency) => <option key={currency}>{currency}</option>)}</select></label>}</div></div>{stats.heatmap.length === 0 ? <div className="empty">{t('usage.noHeatmapData')}</div> : <><EChart ariaLabel={t('usage.heatmapLabel')} className="usage-echart-heatmap" locale={locale} option={heatmap} timeZone={stats.time_zone} onClick={({ dataIndex }) => setSelectedHeatHour(stats.heatmap[dataIndex]?.hour_of_week)} /><HeatmapDataTable currency={effectiveHeatCurrency} format={chartFormatters} metric={heatMetric} onSelect={(value) => setSelectedHeatHour(value.hour_of_week)} summary={copy.charts} timeZone={stats.time_zone} valueLabel={heatMetricLabel} values={stats.heatmap} weekdays={weekdays} />{selectedHeatCell && <div className="usage-heatmap-selection" role="status">{copy.selectedCell}: {weekdays[Math.floor(selectedHeatCell.hour_of_week / 24)]} {String(selectedHeatCell.hour_of_week % 24).padStart(2, '0')}:00 · {heatMetric === 'failure_rate' ? formatPercent(heatmapValue(selectedHeatCell, heatMetric, effectiveHeatCurrency), locale) : heatMetric === 'cost' ? formatCurrency(heatmapValue(selectedHeatCell, heatMetric, effectiveHeatCurrency), effectiveHeatCurrency, locale) : formatNumber(heatmapValue(selectedHeatCell, heatMetric, effectiveHeatCurrency), locale)}</div>}</>}</article>}
    </section></Suspense>}
    {!loading && token.trim() && !error && !stats && <div className="empty">{t('usage.noData')}</div>}
  </div>;
}
