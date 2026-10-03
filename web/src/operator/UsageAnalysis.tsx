import { displayTimeZone, bucketTimeZoneNote } from '../charts/displayTimeZone';
import { lazy, Suspense, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { api } from '../api';
import { Button, Input, Select } from '../design-system';
import { ChartDataView } from '../charts/ChartDataView';
import { HeatmapDataTable } from '../charts/HeatmapDataTable';
import {
  costCurrencies, costOption, heatmapOption, heatmapValue, latencyOption,
  throughputOption, totalTokens, type HeatmapMetric, type UsageChartCopy, type UsageChartFormatters,
} from '../charts/usageCharts';
import { formatCurrencyDisplay, formatMetricDisplay, formatNumber, formatPercent } from '../format';
import { LocalSettlementNotice, localSettlementLabel, localSettlementTrendLabel } from '../LocalSettlementNotice';
import { analyticsDuration, finiteP95Points, histogramP95 } from './analyticsPresentation';
import { UsageSummaryMetrics } from './UsageSummaryMetrics';
import { useI18n } from '../i18n';
import type { KeyView, OperatorUsageAnalysis, TypedFilterAst, UsageAnalysisBucket, UsageAnalysisCost, UsageAnalysisMetrics, UsageAnalysisSessionBucket, UsageAnalysisTimeBucket, UpstreamAccount } from '../types';
import './usage.css';
import { TypedFilterBuilder } from './TypedFilterBuilder';
import { emptyTypedFilterAst } from './traffic/requestTraffic';
import { defaultUsageSelection, localDateTimeInput, nextUsageTab, statsQuery, usageAstFromSelection, usagePresets, usageTabs, type UsageFilters, type UsageSelection, type UsageTab } from './usageState';

const EChart = lazy(() => import('../charts/EChart').then((module) => ({ default: module.EChart })));

type Dimension = 'models' | 'keys' | 'sessions' | 'upstreams' | 'protocols' | 'statuses' | 'errors';
const dimensions: Dimension[] = ['models', 'keys', 'sessions', 'upstreams', 'protocols', 'statuses', 'errors'];
const localCopy = {
  'zh-CN': {
    dimensions: '维度分析', filters: '筛选条件', charts: '图表数据', throughput: '请求吞吐', latency: '响应延迟', costTrend: '费用趋势',
    averageLatency: '平均延迟', p95Latency: 'P95 延迟（近似）', heatMetric: '热力指标', requests: '请求数', success: '成功', failures: '失败',
    tokens: '词元', cost: '费用', failureRate: '失败率', selectedCell: '已选择', chartEmpty: '当前范围没有可绘制的数据', filtersActive: '项筛选已生效', category: '分类',
  },
  en: {
    dimensions: 'Dimensions', filters: 'Filters', charts: 'Chart data', throughput: 'Request throughput', latency: 'Response latency', costTrend: 'Cost trend',
    averageLatency: 'Average latency', p95Latency: 'P95 latency (approx.)', heatMetric: 'Heat metric', requests: 'Requests', success: 'Successful', failures: 'Failed',
    tokens: 'Tokens', cost: 'Cost', failureRate: 'Failure rate', selectedCell: 'Selected', chartEmpty: 'No chartable data in this range', filtersActive: 'active filters', category: 'Category',
  },
} as const;

function UsageCredentialAliasSelect({ disabled, onChange, tenant, token, value }: { disabled: boolean; onChange: (value: string) => void; tenant: string; token: string; value: string }) {
  const { locale, t } = useI18n();
  const [credentials, setCredentials] = useState<KeyView[]>([]);
  const [search, setSearch] = useState('');
  const [cursor, setCursor] = useState<{ created_at: number; key_id: string }>();
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => {
    setCredentials([]); setCursor(undefined); setHasMore(false);
  }, [tenant, token, search]);
  useEffect(() => {
    if (!token.trim()) { setCredentials([]); return; }
    const controller = new AbortController();
    const params = new URLSearchParams({ limit: '100' });
    if (tenant) params.set('tenant_external_id', tenant);
    if (search.trim()) params.set('search', search.trim());
    if (cursor) { params.set('before_created_at', String(cursor.created_at)); params.set('before_id', cursor.key_id); }
    setLoading(true); setError('');
    void api<KeyView[]>(`/internal/v1/keys?${params}`, token.trim(), { signal: controller.signal })
      .then((values) => { if (!controller.signal.aborted) { setCredentials((previous) => cursor ? [...previous, ...values] : values); setHasMore(values.length === 100); } })
      .catch((reason: unknown) => { if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : t('common.requestFailed')); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [tenant, token, search, cursor, t]);
  const aliases = [...new Set([...credentials.map((credential) => credential.alias.trim()), value].filter(Boolean))].sort((left, right) => left.localeCompare(right));
  return <div><label>{t('usage.clientCredential')}<Select value={value} disabled={disabled} onChange={(_, data) => onChange(data.value)}><option value="">{t('common.all')}</option>{aliases.map((alias) => <option key={alias} value={alias}>{alias}</option>)}</Select></label>
    <Input aria-label={t('routes.searchCredentials')} placeholder={t('routes.searchCredentials')} value={search} disabled={disabled} onChange={(_, data) => setSearch(data.value)} />
    {error && <span role="alert">{error}</span>}{hasMore && <Button type="button" appearance="subtle" disabled={disabled || loading} onClick={() => { const last = credentials.at(-1); if (last) setCursor({ created_at: last.created_at, key_id: last.key_id }); }}>{locale === 'zh-CN' ? '加载更多凭据' : 'Load more credentials'}</Button>}
  </div>;
}

const formatCurrency = (value: string | number, currency: string, locale: 'en' | 'zh-CN') => formatCurrencyDisplay(value, currency, locale).text;
const formatMilliseconds = (value: number | null, locale: 'en' | 'zh-CN') => analyticsDuration(value, locale).text;
function CostValue({ costs }: { costs: UsageAnalysisCost[] }) {
  const { locale } = useI18n(); if (!costs.length) return <span>—</span>;
  return <span className="usage-cost-lines">{[...costs].sort((a, b) => a.currency.localeCompare(b.currency)).map(({ currency, cost }) => <span key={currency} title={`${cost} ${currency}`}>{formatCurrency(cost, currency, locale)}</span>)}</span>;
}
function successRate(metrics: UsageAnalysisMetrics) { return metrics.requests > 0 ? metrics.success / metrics.requests : undefined; }

function localizedBucketInterval(epoch: number, duration: number, locale: 'en' | 'zh-CN', timeZone: string) {
  const options: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false, timeZone };
  const language = locale === 'en' ? 'en-US' : 'zh-CN';
  return `[${new Date(epoch).toLocaleString(language, options)}, ${new Date(epoch + duration).toLocaleString(language, options)})`;
}

function GenerationBreakdown({ stats }: { stats: OperatorUsageAnalysis }) {
  const { locale, t } = useI18n(); const copy = localCopy[locale];
  const rows = [
    ...(stats.generation_units_by_modality ?? []).map((value) => ({ id: `modality:${value.modality}:${value.currency}`, group: t('usage.generationByModality'), name: t(`modality.${value.modality}`), currency: value.currency, units: value.units })),
    ...(stats.generation_units_by_billing_unit ?? []).map((value) => ({ id: `unit:${value.billing_unit}:${value.currency}`, group: t('usage.generationByBillingUnit'), name: t(`billingUnit.${value.billing_unit}`), currency: value.currency, units: value.units })),
  ];
  if (!rows.length) return null;
  return <article className="panel usage-generation-table"><div className="table-scroll"><table><thead><tr><th>{t('usage.generationBreakdown')}</th><th>{copy.category}</th><th>{t('usage.generationUnits')}</th></tr></thead><tbody>{rows.map((row) => { const units = formatMetricDisplay(row.units, locale); return <tr key={row.id}><td>{row.group}</td><td>{row.name} · {row.currency}</td><td title={units.title}>{units.text}</td></tr>; })}</tbody></table></div></article>;
}

function DimensionTable<T extends UsageAnalysisBucket>({ title, values, onSelect, labelForValue }: { title: string; values: T[]; onSelect?: (value: T) => void; labelForValue?: (value: T) => string }) {
  const { locale, t } = useI18n();
  return <article className="panel usage-dimension"><div className="panel-title"><h2>{title}</h2><span>{formatNumber(values.length, locale)}</span></div>
    {values.length === 0 ? <div className="empty">{t('usage.noDimensionData')}</div> : <div className="table-scroll"><table><thead><tr><th>{title}</th><th>{t('usage.requests')}</th><th>{t('usage.tokens')}</th><th>{t('usage.generationUnits')}</th><th><LocalSettlementNotice /></th><th>{t('usage.successRate')}</th></tr></thead><tbody>{values.map((value) => {
      const label = labelForValue?.(value) || value.label || t('common.none');
      const requestDisplay = formatMetricDisplay(value.requests, locale); const tokenDisplay = formatMetricDisplay(totalTokens(value), locale); const unitDisplay = formatMetricDisplay(value.generation_units, locale);
      return <tr key={value.id}><td>{onSelect ? <Button type="button" appearance="transparent" className="table-link usage-filter-link" onClick={() => onSelect(value)}>{label}</Button> : label}</td><td title={requestDisplay.title}>{requestDisplay.text}</td><td title={tokenDisplay.title}>{tokenDisplay.text}</td><td title={unitDisplay.title}>{unitDisplay.text}</td><td><CostValue costs={value.costs} /></td><td>{formatPercent(successRate(value), locale)}</td></tr>;
    })}</tbody></table></div>}
  </article>;
}

function ChartTable({ onSelect, points, timeZone }: { onSelect?: (point: UsageAnalysisTimeBucket) => void; points: UsageAnalysisTimeBucket[]; timeZone: string }) {
  const { locale, t } = useI18n(); const copy = localCopy[locale];
  return <div className="usage-chart-table" aria-label={copy.charts}><div className="table-scroll"><table><thead><tr><th>{t('traffic.from')} · {timeZone}</th><th>{copy.success}</th><th>{copy.failures}</th><th>{copy.tokens}</th><th>{copy.averageLatency}</th><th>{copy.p95Latency}</th><th><LocalSettlementNotice /></th></tr></thead><tbody>{points.map((point) => { const label = new Date(point.bucket_start).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN', { timeZone }); const success = formatMetricDisplay(point.success, locale); const failed = formatMetricDisplay(point.failed, locale); const tokens = formatMetricDisplay(totalTokens(point), locale); return <tr key={point.bucket_start}><td>{onSelect ? <Button type="button" appearance="transparent" className="table-link" onClick={() => onSelect(point)}>{label}</Button> : label}</td><td title={success.title}>{success.text}</td><td title={failed.title}>{failed.text}</td><td title={tokens.title}>{tokens.text}</td><td title={analyticsDuration(point.avg_duration_ms, locale).title}>{analyticsDuration(point.avg_duration_ms, locale).text}</td><td title={histogramP95(point.p95_duration_ms, point.p95_is_capped, locale).title}>{histogramP95(point.p95_duration_ms, point.p95_is_capped, locale).text}</td><td><CostValue costs={point.costs} /></td></tr>; })}</tbody></table></div></div>;
}
function ChartCard({ title, children, className = '', onSelect, points, timeZone }: { title: string; children: ReactNode; className?: string; onSelect?: (point: UsageAnalysisTimeBucket) => void; points: UsageAnalysisTimeBucket[]; timeZone: string }) {
  const { locale } = useI18n();
  return <article className={`panel usage-chart-card ${className}`}><ChartDataView title={title} metadata={<span>{timeZone}</span>} data={<ChartTable onSelect={onSelect} points={points} timeZone={timeZone} />}>{points.length === 0 ? <div className="empty">{localCopy[locale].chartEmpty}</div> : children}</ChartDataView></article>;
}
function LazyChart({ children }: { children: ReactNode }) {
  const { t } = useI18n();
  return <Suspense fallback={<div className="empty">{t('common.loading')}</div>}>{children}</Suspense>;
}

export function UsageAnalysis({ token, tenant, upstreams, onOpenSession }: { token: string; tenant: string; upstreams: UpstreamAccount[]; onOpenSession: (session: UsageAnalysisSessionBucket) => void }) {
  const { locale, t } = useI18n(); const copy = localCopy[locale];
  const [tab, setTab] = useState<UsageTab>('overview'); const [dimension, setDimension] = useState<Dimension>('models');
  const [heatMetric, setHeatMetric] = useState<HeatmapMetric>('requests'); const [heatCurrency, setHeatCurrency] = useState(''); const [selectedHeatHour, setSelectedHeatHour] = useState<number>();
  const [selection, setSelection] = useState<UsageSelection>(() => defaultUsageSelection());
  const [typedFilters, setTypedFilters] = useState<TypedFilterAst>(() => usageAstFromSelection(emptyTypedFilterAst, selection)!);
  const [applied, setApplied] = useState(selection); const [refresh, setRefresh] = useState(0); const scope = useMemo(() => ({}), [token, tenant, applied, refresh]);
  const [remote, setRemote] = useState<{ scope: object; status: 'loading' } | { scope: object; status: 'ready'; value: OperatorUsageAnalysis } | { scope: object; status: 'error'; message: string }>(); const requestSequence = useRef(0);

  const refreshUsage = () => { const next = selection.preset === 'custom' ? selection : { ...selection, resolvedAt: Date.now() }; setSelection(next); setApplied(next); setTypedFilters(usageAstFromSelection(emptyTypedFilterAst, next)!); setRefresh((value) => value + 1); };

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

  const applyDimension = (filter: keyof UsageFilters, bucket: UsageAnalysisBucket) => {
    const next = { ...selection, filters: { ...selection.filters, [filter]: bucket.id } };
    setTypedFilters(usageAstFromSelection(emptyTypedFilterAst, next)!);
    setSelection(next); setApplied(next);
  };
  const selectUtcBucket = (point: UsageAnalysisTimeBucket) => { const millis = stats?.granularity === 'hour' ? 3_600_000 : 86_400_000; const next = { ...selection, preset: 'custom' as const, customFrom: localDateTimeInput(point.bucket_start), customTo: localDateTimeInput(point.bucket_start + millis - 1) }; setTypedFilters(usageAstFromSelection(emptyTypedFilterAst, next)!); setSelection(next); setApplied(next); setTab('overview'); };

  const chartCopy: UsageChartCopy = useMemo(() => ({ requests: copy.requests, success: copy.success, failures: copy.failures, averageLatency: copy.averageLatency, p95Latency: copy.p95Latency, cost: localSettlementLabel(locale), noData: copy.chartEmpty }), [copy, locale]);
  const chartFormatters: UsageChartFormatters = useMemo(() => {
    const timeZone = displayTimeZone();
    const duration = stats?.granularity === 'day' ? 86_400_000 : 3_600_000;
    const language = locale === 'en' ? 'en-US' : 'zh-CN';
    const options: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false, timeZone };
    return {
      bucket: (epoch) => new Date(epoch).toLocaleString(language, options),
      bucketInterval: (epoch) => localizedBucketInterval(epoch, duration, locale, timeZone),
      cost: (value, currency) => formatCurrency(value, currency, locale), duration: (value) => formatMilliseconds(value, locale), number: (value) => formatMetricDisplay(value, locale).text, percent: (value) => formatPercent(value, locale),
    };
  }, [locale, stats?.granularity]);
  const throughput = useMemo(() => throughputOption(stats?.time_series ?? [], chartCopy, chartFormatters), [stats?.time_series, chartCopy, chartFormatters]);
  const latency = useMemo(() => latencyOption(finiteP95Points(stats?.time_series ?? []), chartCopy, chartFormatters), [stats?.time_series, chartCopy, chartFormatters]);
  const costs = useMemo(() => costOption(stats?.time_series ?? [], chartCopy, chartFormatters), [stats?.time_series, chartCopy, chartFormatters]);
  const currencies = stats ? costCurrencies(stats.heatmap) : []; const effectiveHeatCurrency = currencies.includes(heatCurrency) ? heatCurrency : (currencies[0] ?? 'USD');
  const weekdays = useMemo(() => Array.from({ length: 7 }, (_, day) => new Date(Date.UTC(2024, 0, 8 + day)).toLocaleDateString(locale === 'en' ? 'en-US' : 'zh-CN', { weekday: 'short', timeZone: stats?.time_zone ?? 'UTC' })), [locale, stats?.time_zone]);
  const heatmap = useMemo(() => heatmapOption(stats?.heatmap ?? [], heatMetric, effectiveHeatCurrency, weekdays, t('usage.heatmapLabel'), chartFormatters), [stats?.heatmap, heatMetric, effectiveHeatCurrency, weekdays, t, chartFormatters]);
  const externalFilterChips = [
    ...(applied.filters.upstreamId === 'unassigned' ? [{ id: 'usage-upstream-unassigned', label: `${t('filter.field.upstream_account_id')} ${t('filter.operator.equals')} ${t('usage.unassigned')}` }] : []),
  ];
  const heatMetricLabel = heatMetric === 'requests' ? copy.requests : heatMetric === 'tokens' ? copy.tokens : heatMetric === 'cost' ? localSettlementLabel(locale) : copy.failureRate;
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

  const reportedRange = stats
    ? `${new Date(stats.from_created_at).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN', { timeZone: displayTimeZone() })} – ${new Date(stats.to_created_at).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN', { timeZone: displayTimeZone() })}`
    : undefined;

  return <div className="usage-page"><div className="usage-heading"><div><h2>{t('usage.title')}</h2><p className="muted">{t('usage.description')}</p>{stats && <span className="usage-time-zone" title={reportedRange}>{bucketTimeZoneNote(locale, stats.time_zone)}</span>}</div><Button type="button" appearance="secondary" disabled={loading || !token.trim()} onClick={refreshUsage}>{loading ? t('common.loading') : t('usage.refresh')}</Button></div>
    <TypedFilterBuilder ast={typedFilters} disabled={Boolean(loading) || !token.trim()} externalChips={externalFilterChips} panelActive={selection.preset !== '24h' || selection.granularity !== 'auto'} usageSelection={selection} panelControls={(draft, setDraft) => <div className="usage-controls">
      <fieldset><legend>{t('usage.timeRange')}</legend><div className="usage-presets">{usagePresets.map((preset) => <Button type="button" appearance="secondary" className={draft.preset === preset ? 'active' : undefined} aria-pressed={draft.preset === preset} key={preset} onClick={() => setDraft({ ...draft, preset, resolvedAt: Date.now() })}>{t(`usage.preset.${preset}`)}</Button>)}</div></fieldset>
      {draft.preset === 'custom' && <div className="usage-custom-range"><label>{t('traffic.from')}<Input type="datetime-local" step="0.001" value={draft.customFrom} onChange={(_, data) => setDraft({ ...draft, customFrom: data.value })} /></label><label>{t('traffic.to')}<Input type="datetime-local" step="0.001" value={draft.customTo} onChange={(_, data) => setDraft({ ...draft, customTo: data.value })} /></label></div>}
      <div className="usage-filter-grid"><label>{t('usage.granularity')}<Select value={draft.granularity} onChange={(_, data) => setDraft({ ...draft, granularity: data.value as UsageSelection['granularity'] })}><option value="auto">{t('usage.granularity.auto')}</option><option value="hour">{t('usage.granularity.hour')}</option><option value="day">{t('usage.granularity.day')}</option></Select></label><UsageCredentialAliasSelect disabled={Boolean(loading) || !token.trim()} token={token} tenant={tenant} value={draft.filters.keyAlias} onChange={(keyAlias) => setDraft({ ...draft, filters: { ...draft.filters, keyAlias } })} /></div>
    </div>} onApply={(ast, next) => { if (!next) return; setTypedFilters(ast); setSelection(next); setApplied(next); }} onClear={() => { const next = defaultUsageSelection(); setTypedFilters(usageAstFromSelection(emptyTypedFilterAst, next)!); setSelection(next); setApplied(next); }} scope="usage" token={token} tenant={tenant} upstreams={upstreams} />
    {!token.trim() && <div className="notice warning" role="status">{t('usage.connectPrompt')}</div>}{loading && <div className="notice" role="status">{t('common.loading')}</div>}{error && <div className="notice error" role="alert">{error}</div>}
    <nav className="usage-tabs" role="tablist" aria-label={t('usage.sections')}>{usageTabs.map((id) => <Button type="button" role="tab" id={`usage-tab-${id}`} aria-controls={`usage-panel-${id}`} aria-selected={tab === id} tabIndex={tab === id ? 0 : -1} className={tab === id ? 'active' : ''} key={id} onClick={() => setTab(id)} onKeyDown={(event) => { const next = nextUsageTab(id, event.key); if (!next) return; event.preventDefault(); setTab(next); requestAnimationFrame(() => document.getElementById(`usage-tab-${next}`)?.focus()); }}>{id === 'dimensions' ? copy.dimensions : t(`usage.tab.${id}`)}</Button>)}</nav>
    {stats && <section className="usage-tab-panel" role="tabpanel" id={`usage-panel-${tab}`} aria-labelledby={`usage-tab-${tab}`}><p className="analytics-settlement-note"><LocalSettlementNotice /></p>{tab === 'overview' && <><UsageSummaryMetrics stats={stats} /><p className="analytics-p95-note">{locale === 'zh-CN' ? 'P95 按直方图区间上界展示；超出范围及早期无法确定的值留空。' : 'P95 uses histogram upper bounds. Out-of-range and ambiguous earlier values leave gaps.'}</p><GenerationBreakdown stats={stats} /><div className="usage-overview-chart-grid"><ChartCard className="usage-chart-primary" title={copy.throughput} onSelect={selectUtcBucket} points={stats.time_series} timeZone={displayTimeZone()}><LazyChart><EChart ariaLabel={copy.throughput} locale={locale} option={throughput} timeZone={displayTimeZone()} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></LazyChart></ChartCard><ChartCard title={copy.latency} onSelect={selectUtcBucket} points={stats.time_series} timeZone={displayTimeZone()}><LazyChart><EChart ariaLabel={copy.latency} locale={locale} option={latency} timeZone={displayTimeZone()} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></LazyChart></ChartCard><ChartCard title={localSettlementTrendLabel(locale)} onSelect={selectUtcBucket} points={stats.time_series} timeZone={displayTimeZone()}><LazyChart><EChart ariaLabel={localSettlementTrendLabel(locale)} locale={locale} option={costs} timeZone={displayTimeZone()} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></LazyChart></ChartCard></div></>}
      {tab === 'trend' && <div className="usage-chart-grid"><ChartCard title={copy.throughput} onSelect={selectUtcBucket} points={stats.time_series} timeZone={displayTimeZone()}><LazyChart><EChart ariaLabel={copy.throughput} locale={locale} option={throughput} timeZone={displayTimeZone()} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></LazyChart></ChartCard><ChartCard title={copy.latency} onSelect={selectUtcBucket} points={stats.time_series} timeZone={displayTimeZone()}><LazyChart><EChart ariaLabel={copy.latency} locale={locale} option={latency} timeZone={displayTimeZone()} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></LazyChart></ChartCard><ChartCard title={localSettlementTrendLabel(locale)} onSelect={selectUtcBucket} points={stats.time_series} timeZone={displayTimeZone()}><LazyChart><EChart ariaLabel={localSettlementTrendLabel(locale)} locale={locale} option={costs} timeZone={displayTimeZone()} onClick={({ dataIndex }) => stats.time_series[dataIndex] && selectUtcBucket(stats.time_series[dataIndex])} /></LazyChart></ChartCard></div>}
      {tab === 'dimensions' && <><div className="usage-dimension-picker" role="tablist" aria-label={copy.dimensions}>{dimensions.map((id) => <Button key={id} type="button" className={dimension === id ? 'active' : 'secondary'} aria-pressed={dimension === id} onClick={() => setDimension(id)}>{t(`usage.${id}`)}</Button>)}</div>{renderDimension()}</>}
      {tab === 'heatmap' && <article className="panel usage-heatmap-panel"><ChartDataView title={t('usage.heatmap')} metadata={<div className="usage-heatmap-controls"><span>{stats.time_zone}</span><label>{copy.heatMetric}<Select value={heatMetric} onChange={(_, data) => { setHeatMetric(data.value as HeatmapMetric); setSelectedHeatHour(undefined); }}><option value="requests">{copy.requests}</option><option value="tokens">{copy.tokens}</option><option value="cost">{copy.cost}</option><option value="failure_rate">{copy.failureRate}</option></Select></label>{heatMetric === 'cost' && <label>{t('usage.trendCurrency')}<Select value={effectiveHeatCurrency} onChange={(_, data) => { setHeatCurrency(data.value); setSelectedHeatHour(undefined); }}>{currencies.map((currency) => <option key={currency}>{currency}</option>)}</Select></label>}</div>} data={<HeatmapDataTable currency={effectiveHeatCurrency} format={chartFormatters} metric={heatMetric} onSelect={(value) => setSelectedHeatHour(value.hour_of_week)} summary={copy.charts} timeZone={stats.time_zone} valueLabel={heatMetricLabel} values={stats.heatmap} weekdays={weekdays} />}>{stats.heatmap.length === 0 ? <div className="empty">{t('usage.noHeatmapData')}</div> : <LazyChart><EChart ariaLabel={t('usage.heatmapLabel')} className="usage-echart-heatmap" locale={locale} option={heatmap} timeZone={stats.time_zone} onClick={({ dataIndex }) => setSelectedHeatHour(stats.heatmap[dataIndex]?.hour_of_week)} /></LazyChart>}</ChartDataView>{selectedHeatCell && <div className="usage-heatmap-selection" role="status">{copy.selectedCell}: {weekdays[Math.floor(selectedHeatCell.hour_of_week / 24)]} {String(selectedHeatCell.hour_of_week % 24).padStart(2, '0')}:00 · {heatMetric === 'failure_rate' ? formatPercent(heatmapValue(selectedHeatCell, heatMetric, effectiveHeatCurrency), locale) : heatMetric === 'cost' ? formatCurrency(heatmapValue(selectedHeatCell, heatMetric, effectiveHeatCurrency), effectiveHeatCurrency, locale) : formatMetricDisplay(heatmapValue(selectedHeatCell, heatMetric, effectiveHeatCurrency), locale).text}</div>}</article>}
    </section>}
    {!loading && token.trim() && !error && !stats && <div className="empty">{t('usage.noData')}</div>}
  </div>;
}
