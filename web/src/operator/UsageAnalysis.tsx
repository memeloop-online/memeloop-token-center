import { useEffect, useRef, useState, type CSSProperties } from 'react';
import { api } from '../api';
import { formatCurrency, formatMetricNumber, formatMilliseconds, formatNumber, formatPercent } from '../format';
import { Metric } from '../components';
import { useI18n } from '../i18n';
import type {
  OperatorUsageAnalysis, UsageAnalysisBucket, UsageAnalysisCost, UsageAnalysisHeatmapBucket,
  UsageAnalysisMetrics, UsageAnalysisSessionBucket, UsageAnalysisTimeBucket, UpstreamAccount,
} from '../types';
import { trendCurrencies, trendMetrics, trendValue, type TrendMetric } from './usageTrend';

type UsageTab = 'overview' | 'trend' | 'models' | 'keys' | 'sessions' | 'upstreams' | 'heatmap';
type Preset = '24h' | 'today' | 'yesterday' | '7d' | '30d' | 'custom';
type Granularity = 'auto' | 'hour' | 'day';

interface UsageFilters {
  model: string;
  keyId: string;
  upstreamId: string;
  protocol: string;
  status: string;
  errorCode: string;
}

interface UsageSelection {
  preset: Preset;
  granularity: Granularity;
  customFrom: string;
  customTo: string;
  filters: UsageFilters;
}

const usageTabs: UsageTab[] = ['overview', 'trend', 'models', 'keys', 'sessions', 'upstreams', 'heatmap'];
const presets: Preset[] = ['24h', 'today', 'yesterday', '7d', '30d', 'custom'];
const emptyFilters: UsageFilters = {
  model: '', keyId: '', upstreamId: '', protocol: '', status: '', errorCode: '',
};

function localDateTimeInput(epoch: number) {
  const date = new Date(epoch);
  const local = new Date(epoch - date.getTimezoneOffset() * 60_000);
  return local.toISOString().slice(0, 23);
}

function rangeFor(selection: UsageSelection, now = Date.now()) {
  const end = now;
  if (selection.preset === '24h') return { from: end - 86_400_000, to: end };
  if (selection.preset === '7d') return { from: end - 7 * 86_400_000, to: end };
  if (selection.preset === '30d') return { from: end - 30 * 86_400_000, to: end };
  const today = new Date(now);
  today.setHours(0, 0, 0, 0);
  if (selection.preset === 'today') return { from: today.getTime(), to: end };
  if (selection.preset === 'yesterday') {
    const yesterday = new Date(today);
    yesterday.setDate(yesterday.getDate() - 1);
    return { from: yesterday.getTime(), to: today.getTime() - 1 };
  }
  const from = Date.parse(selection.customFrom);
  const to = Date.parse(selection.customTo);
  if (!Number.isFinite(from) || !Number.isFinite(to) || from > to) return undefined;
  return { from, to };
}

function statsQuery(tenant: string, selection: UsageSelection) {
  const range = rangeFor(selection);
  if (!range) return undefined;
  const params = new URLSearchParams({
    from_created_at: String(range.from),
    to_created_at: String(range.to),
    granularity: selection.granularity,
  });
  if (tenant) params.set('tenant_external_id', tenant);
  if (selection.filters.model.trim()) params.set('model', selection.filters.model.trim());
  if (selection.filters.keyId.trim()) params.set('key_id', selection.filters.keyId.trim());
  if (selection.filters.upstreamId) params.set('upstream_account_id', selection.filters.upstreamId);
  if (selection.filters.protocol) params.set('protocol', selection.filters.protocol);
  if (selection.filters.status) params.set('status', selection.filters.status);
  if (selection.filters.errorCode.trim()) params.set('error_code', selection.filters.errorCode.trim());
  return `?${params}`;
}

function NumericMetric({ label, value, tone }: { label: string; value?: number | null; tone?: string }) {
  const { locale } = useI18n();
  const formatted = formatMetricNumber(value, locale);
  return <Metric label={label} tone={tone} value={<span title={formatted.title}>{formatted.text}</span>} />;
}

function CostValue({ costs }: { costs: UsageAnalysisCost[] }) {
  const { locale } = useI18n();
  if (!costs.length) return <span>—</span>;
  return <span className="usage-cost-lines">{[...costs].sort((left, right) => left.currency.localeCompare(right.currency)).map(({ currency, cost }) => <span key={currency} title={`${cost} ${currency}`}>{formatCurrency(cost, currency, locale)}</span>)}</span>;
}

function generationUnitLabel(kind: 'modality' | 'billing_unit', value: string, t: (key: string) => string) {
  return t(kind === 'modality' ? `modality.${value}` : `billingUnit.${value}`);
}

function GenerationUnitsValue({ modalityValues = [], billingUnitValues = [], legacyUnits = 0, mode = 'both' }: {
  modalityValues?: NonNullable<OperatorUsageAnalysis['generation_units_by_modality']>;
  billingUnitValues?: NonNullable<OperatorUsageAnalysis['generation_units_by_billing_unit']>;
  legacyUnits?: number;
  mode?: 'modality' | 'billing_unit' | 'both';
}) {
  const { locale, t } = useI18n();
  const rows = [
    ...(mode !== 'billing_unit' ? modalityValues.map((item) => ({ kind: 'modality' as const, value: item.modality, currency: item.currency, units: item.units })) : []),
    ...(mode !== 'modality' ? billingUnitValues.map((item) => ({ kind: 'billing_unit' as const, value: item.billing_unit, currency: item.currency, units: item.units })) : []),
  ].sort((left, right) => `${left.kind}:${left.value}:${left.currency}`.localeCompare(`${right.kind}:${right.value}:${right.currency}`));
  if (!rows.length) {
    return legacyUnits > 0
      ? <span title={String(legacyUnits)}>{formatNumber(legacyUnits, locale)} · {t('usage.legacyGenerationUnits')}</span>
      : <span>—</span>;
  }
  return <span className="usage-unit-lines">{rows.map((row) => <span key={`${row.kind}:${row.value}:${row.currency}`} title={`${row.units} ${row.currency}`}>
    {generationUnitLabel(row.kind, row.value, t)} · {row.currency} · {formatNumber(row.units, locale)}
  </span>)}</span>;
}

function successRate(metrics: UsageAnalysisMetrics) {
  return metrics.requests > 0 ? metrics.success / metrics.requests : undefined;
}

function DimensionTable<T extends UsageAnalysisBucket>({ title, values, onSelect, labelForValue }: {
  title: string;
  values: T[];
  onSelect?: (value: T) => void;
  labelForValue?: (value: T) => string;
}) {
  const { locale, t } = useI18n();
  return <article className="panel usage-dimension"><div className="panel-title"><h2>{title}</h2><span>{formatNumber(values.length, locale)}</span></div>
    {values.length === 0 ? <div className="empty">{t('usage.noDimensionData')}</div> : <div className="table-scroll"><table><thead><tr><th>{title}</th><th>{t('usage.requests')}</th><th>{t('usage.tokens')}</th><th>{t('usage.cost')}</th><th>{t('usage.successRate')}</th></tr></thead><tbody>{values.map((value) => {
      const label = labelForValue?.(value) || value.label || t('common.none');
      return <tr key={value.id}>
      <td>{onSelect ? <button type="button" className="table-link usage-filter-link" onClick={() => onSelect(value)} aria-label={t('usage.filterByDimension', { name: label })}>{label}</button> : label}</td>
      <td title={String(value.requests)}>{formatNumber(value.requests, locale)}</td>
      <td title={String(value.input_tokens + value.output_tokens + value.cached_input_tokens + value.cache_write_tokens)}>{formatNumber(value.input_tokens + value.output_tokens + value.cached_input_tokens + value.cache_write_tokens, locale)}</td>
      <td><CostValue costs={value.costs} /></td>
      <td>{formatPercent(successRate(value), locale)}</td>
    </tr>;
    })}</tbody></table></div>}
  </article>;
}

function pointEpoch(point: UsageAnalysisTimeBucket) {
  return point.bucket_start;
}

function pointLabel(point: UsageAnalysisTimeBucket, locale: 'zh-CN' | 'en', granularity: 'hour' | 'day') {
  return new Date(point.bucket_start).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN', granularity === 'hour'
    ? { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false, timeZone: 'UTC' }
    : { year: 'numeric', month: 'short', day: 'numeric', timeZone: 'UTC' });
}

function trendFormattedValue(value: number, metric: TrendMetric, currency: string, locale: 'zh-CN' | 'en') {
  if (metric === 'cost') return formatCurrency(value, currency, locale);
  if (metric === 'avg_latency' || metric === 'p95_latency') return formatMilliseconds(value, locale);
  return formatNumber(value, locale, metric === 'tokens' ? 0 : 2);
}

function TrendChart({ points, granularity, metric, currency, onSelectBucket }: {
  points: UsageAnalysisTimeBucket[];
  granularity: 'hour' | 'day';
  metric: TrendMetric;
  currency: string;
  onSelectBucket: (point: UsageAnalysisTimeBucket) => void;
}) {
  const { locale, t } = useI18n();
  const valuedPoints = points.map((point) => ({ point, value: trendValue(point, metric, currency) }));
  if (!valuedPoints.some(({ value }) => value !== null)) return <div className="empty">{t('usage.noTrendData')}</div>;
  const width = 760;
  const height = 240;
  const paddingX = 34;
  const paddingY = 25;
  const maximum = Math.max(1, ...valuedPoints.map(({ value }) => value ?? 0));
  const coordinates = valuedPoints.map(({ point, value }, index) => ({
    point,
    value,
    x: points.length === 1 ? width / 2 : paddingX + (index / (points.length - 1)) * (width - paddingX * 2),
    y: value === null ? null : height - paddingY - (value / maximum) * (height - paddingY * 2),
  }));
  let drawing = false;
  const path = coordinates.map(({ x, y }) => {
    if (y === null) { drawing = false; return ''; }
    const command = drawing ? 'L' : 'M';
    drawing = true;
    return `${command} ${x.toFixed(2)} ${y.toFixed(2)}`;
  }).filter(Boolean).join(' ');
  const labelStep = Math.max(1, Math.ceil(points.length / 6));
  const metricLabel = t(`usage.trendMetric.${metric}`);
  const chartLabel = t('usage.trendChartLabel', { metric: metricLabel, currency: metric === 'cost' ? ` (${currency})` : '' });
  return <div className="usage-trend-chart"><svg viewBox={`0 0 ${width} ${height}`} role="img" aria-label={chartLabel}>
    <title>{chartLabel}</title>
    <line className="usage-chart-axis" x1={paddingX} y1={height - paddingY} x2={width - paddingX} y2={height - paddingY} />
    <path className="usage-chart-line" d={path} />
    {coordinates.map(({ point, value, x, y }, index) => <g key={point.bucket_start}>
      {value !== null && y !== null && <circle className="usage-chart-point" cx={x} cy={y} r="4"><title>{`${pointLabel(point, locale, granularity)}: ${trendFormattedValue(value, metric, currency, locale)}`}</title></circle>}
      {(index % labelStep === 0 || index === coordinates.length - 1) && <text className="usage-chart-label" x={x} y={height - 6} textAnchor={index === 0 ? 'start' : index === coordinates.length - 1 ? 'end' : 'middle'}>{pointLabel(point, locale, granularity)}</text>}
    </g>)}
  </svg><div className="usage-trend-points" aria-label={t('usage.trendData')}>
    {coordinates.map(({ point, value }) => <button type="button" className="secondary" key={point.bucket_start} onClick={() => onSelectBucket(point)}>{pointLabel(point, locale, granularity)} · {value === null ? '—' : trendFormattedValue(value, metric, currency, locale)}</button>)}
  </div></div>;
}

function Heatmap({ values }: { values: UsageAnalysisHeatmapBucket[] }) {
  const { locale, t } = useI18n();
  if (!values.length) return <div className="empty">{t('usage.noHeatmapData')}</div>;
  const maximum = Math.max(1, ...values.map((value) => value.requests));
  const cells = new Map(values.map((value) => [value.hour_of_week, value]));
  const days = Array.from({ length: 7 }, (_, day) => new Date(Date.UTC(2024, 0, 8 + day)).toLocaleDateString(locale === 'en' ? 'en-US' : 'zh-CN', { weekday: 'short', timeZone: 'UTC' }));
  return <div className="usage-heatmap-scroll"><div className="usage-heatmap" role="img" aria-label={t('usage.heatmapLabel')}>
    <span />{Array.from({ length: 24 }, (_, hour) => <span className="usage-heatmap-hour" key={hour}>{String(hour).padStart(2, '0')}</span>)}
    {days.flatMap((dayLabel, day) => [<span className="usage-heatmap-row" key={`${dayLabel}-label`}>{dayLabel}</span>, ...Array.from({ length: 24 }, (_, hour) => {
      const cell = cells.get(day * 24 + hour);
      const requests = cell?.requests ?? 0;
      const style = { '--usage-heat': String(requests / maximum) } as CSSProperties;
      return <span className="usage-heatmap-cell" style={style} key={`${dayLabel}-${hour}`} title={t('usage.heatmapCell', { day: dayLabel, hour: String(hour).padStart(2, '0'), count: formatNumber(requests, locale) })} aria-label={t('usage.heatmapCell', { day: dayLabel, hour: String(hour).padStart(2, '0'), count: formatNumber(requests, locale) })} />;
    })])}
  </div></div>;
}

export function UsageAnalysis({ token, tenant, upstreams, onOpenSession }: { token: string; tenant: string; upstreams: UpstreamAccount[]; onOpenSession: (session: UsageAnalysisSessionBucket) => void }) {
  const { locale, t } = useI18n();
  const now = Date.now();
  const [tab, setTab] = useState<UsageTab>('overview');
  const [selection, setSelection] = useState<UsageSelection>({
    preset: '24h', granularity: 'auto',
    customFrom: localDateTimeInput(now - 86_400_000), customTo: localDateTimeInput(now),
    filters: emptyFilters,
  });
  const [applied, setApplied] = useState(selection);
  const [stats, setStats] = useState<OperatorUsageAnalysis>();
  const [trendMetric, setTrendMetric] = useState<TrendMetric>('requests');
  const [trendCurrency, setTrendCurrency] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const requestSequence = useRef(0);

  useEffect(() => {
    const sequence = ++requestSequence.current;
    if (!token.trim()) { setStats(undefined); setLoading(false); setError(''); return; }
    const query = statsQuery(tenant, applied);
    if (!query) { setError(t('usage.invalidRange')); setLoading(false); return; }
    setLoading(true); setError('');
    void api<OperatorUsageAnalysis>(`/internal/v1/usage-analysis${query}`, token.trim()).then((value) => {
      if (sequence === requestSequence.current) setStats(value);
    }).catch((reason: unknown) => {
      if (sequence === requestSequence.current) { setStats(undefined); setError(reason instanceof Error ? reason.message : t('usage.loadFailed')); }
    }).finally(() => { if (sequence === requestSequence.current) setLoading(false); });
  }, [token, tenant, applied, t]);

  const applyDimension = (dimension: keyof UsageFilters, bucket: UsageAnalysisBucket) => {
    const next = { ...selection, filters: { ...selection.filters, [dimension]: bucket.id } };
    setSelection(next); setApplied(next);
  };
  const selectUtcBucket = (point: UsageAnalysisTimeBucket) => {
    const bucketMillis = stats?.granularity === 'hour' ? 3_600_000 : 86_400_000;
    const next = { ...selection, preset: 'custom' as const, customFrom: localDateTimeInput(pointEpoch(point)), customTo: localDateTimeInput(pointEpoch(point) + bucketMillis - 1) };
    setSelection(next); setApplied(next);
  };
  const summarySuccessRate = stats ? successRate(stats.summary) : undefined;
  const availableTrendCurrencies = stats ? trendCurrencies(stats.time_series) : [];
  const effectiveTrendCurrency = availableTrendCurrencies.includes(trendCurrency)
    ? trendCurrency
    : (availableTrendCurrencies[0] ?? '');

  return <div className="usage-page">
    <div className="usage-heading"><div><h2>{t('usage.title')}</h2><p className="muted">{t('usage.description')}</p></div><button type="button" className="secondary" disabled={loading || !token.trim()} onClick={() => setApplied({ ...selection })}>{loading ? t('common.loading') : t('usage.refresh')}</button></div>
    <form className="usage-controls" onSubmit={(event) => { event.preventDefault(); setApplied({ ...selection }); }}>
      <fieldset><legend>{t('usage.timeRange')}</legend><div className="usage-presets">{presets.map((preset) => <button type="button" className={selection.preset === preset ? 'active' : 'secondary'} aria-pressed={selection.preset === preset} key={preset} onClick={() => {
        const next = { ...selection, preset };
        setSelection(next);
        if (preset !== 'custom') setApplied(next);
      }}>{t(`usage.preset.${preset}`)}</button>)}</div></fieldset>
      {selection.preset === 'custom' && <div className="usage-custom-range"><label>{t('traffic.from')}<input type="datetime-local" step="0.001" value={selection.customFrom} onChange={(event) => setSelection({ ...selection, customFrom: event.target.value })} /></label><label>{t('traffic.to')}<input type="datetime-local" step="0.001" value={selection.customTo} onChange={(event) => setSelection({ ...selection, customTo: event.target.value })} /></label></div>}
      <div className="usage-filter-grid">
        <label>{t('usage.granularity')}<select value={selection.granularity} onChange={(event) => setSelection({ ...selection, granularity: event.target.value as Granularity })}><option value="auto">{t('usage.granularity.auto')}</option><option value="hour">{t('usage.granularity.hour')}</option><option value="day">{t('usage.granularity.day')}</option></select></label>
        <label>{t('request.model')}<input value={selection.filters.model} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, model: event.target.value } })} /></label>
        <label>{t('traffic.keyId')}<input value={selection.filters.keyId} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, keyId: event.target.value } })} placeholder="019f…" /></label>
        <label>{t('traffic.upstream')}<select value={selection.filters.upstreamId} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, upstreamId: event.target.value } })}><option value="">{t('common.all')}</option><option value="unassigned">{t('usage.unassigned')}</option>{upstreams.map((value) => <option value={value.id} key={value.id}>{value.name}</option>)}</select></label>
        <label>{t('request.protocol')}<select value={selection.filters.protocol} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, protocol: event.target.value } })}><option value="">{t('common.all')}</option>{['openai', 'anthropic', 'openai-image', 'generation'].map((protocol) => <option value={protocol} key={protocol}>{t(`usage.protocol.${protocol}`)}</option>)}</select></label>
        <label>{t('request.status')}<select value={selection.filters.status} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, status: event.target.value } })}><option value="">{t('common.all')}</option><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option></select></label>
        <label>{t('traffic.errorCode')}<input value={selection.filters.errorCode} onChange={(event) => setSelection({ ...selection, filters: { ...selection.filters, errorCode: event.target.value } })} /></label>
        <div className="filter-actions"><button type="submit" disabled={loading || !token.trim()}>{t('usage.apply')}</button><button type="button" className="secondary" disabled={loading || !Object.values(selection.filters).some(Boolean)} onClick={() => { const next = { ...selection, filters: emptyFilters }; setSelection(next); setApplied(next); }}>{t('usage.clearFilters')}</button></div>
      </div>
      <p className="usage-epoch-hint">{t('usage.epochHint')}</p>
    </form>
    {!token.trim() && <div className="notice warning" role="status">{t('usage.connectPrompt')}</div>}
    {error && <div className="notice error" role="alert">{error}</div>}
    <nav className="usage-tabs" role="tablist" aria-label={t('usage.sections')}>{usageTabs.map((id) => <button type="button" role="tab" aria-selected={tab === id} className={tab === id ? 'active' : ''} key={id} onClick={() => setTab(id)}>{t(`usage.tab.${id}`)}</button>)}</nav>
    {stats && <section className="panel usage-generation-breakdown" aria-label={t('usage.generationBreakdown')}><div><b>{t('usage.generationByModality')}</b><GenerationUnitsValue modalityValues={stats.generation_units_by_modality} legacyUnits={stats.summary.generation_units} mode="modality" /></div><div><b>{t('usage.generationByBillingUnit')}</b><GenerationUnitsValue billingUnitValues={stats.generation_units_by_billing_unit} legacyUnits={stats.summary.generation_units} mode="billing_unit" /></div></section>}
    {stats && <section className="usage-tab-panel" role="tabpanel">
      {tab === 'overview' && <>
        <section className="metrics usage-metrics">
          <NumericMetric label={t('usage.requests')} value={stats.summary.requests} />
          <Metric label={t('usage.successRate')} value={formatPercent(summarySuccessRate, locale)} tone="positive" />
          <NumericMetric label={t('usage.failures')} value={stats.summary.failed} tone="negative" />
          <Metric label={t('usage.cost')} value={<CostValue costs={stats.summary.costs} />} />
          <NumericMetric label={t('usage.totalTokens')} value={stats.summary.input_tokens + stats.summary.output_tokens + stats.summary.cached_input_tokens + stats.summary.cache_write_tokens} />
          <NumericMetric label={t('usage.inputTokens')} value={stats.summary.input_tokens} />
          <NumericMetric label={t('usage.outputTokens')} value={stats.summary.output_tokens} />
          <NumericMetric label={t('usage.cachedTokens')} value={stats.summary.cached_input_tokens} />
          <NumericMetric label={t('usage.cacheWriteTokens')} value={stats.summary.cache_write_tokens} />
          <Metric label={t('usage.p95')} value={formatMilliseconds(stats.summary.p95_duration_ms, locale)} />
          <Metric label={t('usage.average')} value={formatMilliseconds(stats.summary.avg_duration_ms, locale)} />
        </section>
        <section className="two-column usage-overview-tables">
          <DimensionTable title={t('usage.protocols')} values={stats.by_protocol} labelForValue={(bucket) => t(`usage.protocol.${bucket.id}`)} onSelect={(bucket) => applyDimension('protocol', bucket)} />
          <DimensionTable title={t('usage.statuses')} values={stats.by_status} labelForValue={(bucket) => bucket.id === 'success' ? t('traffic.success') : bucket.id === 'error' ? t('traffic.failure') : bucket.label} onSelect={(bucket) => applyDimension('status', bucket)} />
          <DimensionTable title={t('usage.errors')} values={stats.errors} onSelect={(bucket) => applyDimension('errorCode', bucket)} />
        </section>
      </>}
      {tab === 'trend' && <article className="panel"><div className="panel-title usage-trend-title"><h2>{t('usage.trend')}</h2><div className="usage-trend-controls">
        <label>{t('usage.trendMetric')}<select aria-label={t('usage.trendMetric')} value={trendMetric} onChange={(event) => setTrendMetric(event.target.value as TrendMetric)}>{trendMetrics.map((metric) => <option value={metric} key={metric}>{t(`usage.trendMetric.${metric}`)}</option>)}</select></label>
        {trendMetric === 'cost' && <label>{t('usage.trendCurrency')}<select aria-label={t('usage.trendCurrency')} value={effectiveTrendCurrency} disabled={!availableTrendCurrencies.length} onChange={(event) => setTrendCurrency(event.target.value)}>{availableTrendCurrencies.map((currency) => <option value={currency} key={currency}>{currency}</option>)}</select></label>}
        <span>{stats.time_zone} · {t(`usage.granularity.${stats.granularity}`)}</span>
      </div></div><TrendChart points={stats.time_series} granularity={stats.granularity} metric={trendMetric} currency={effectiveTrendCurrency} onSelectBucket={selectUtcBucket} /></article>}
      {tab === 'models' && <DimensionTable title={t('usage.models')} values={stats.by_model} onSelect={(bucket) => applyDimension('model', bucket)} />}
      {tab === 'keys' && <DimensionTable title={t('usage.keys')} values={stats.by_key} onSelect={(bucket) => applyDimension('keyId', bucket)} />}
      {tab === 'sessions' && <><p className="usage-dimension-contract">{t('usage.sessionGrouping')}</p><DimensionTable title={t('usage.sessions')} values={stats.by_session} labelForValue={(bucket) => bucket.unlinked || bucket.id.startsWith('unlinked:') ? t('sessions.unlinkedRequests') : bucket.label} onSelect={onOpenSession} /></>}
      {tab === 'upstreams' && <><p className="usage-dimension-contract">{t(`usage.upstreamGrouping.${stats.upstream_grouping}`)}</p><DimensionTable title={t('usage.upstreams')} values={stats.by_upstream} labelForValue={(bucket) => bucket.id === 'unassigned' ? t('usage.unassigned') : bucket.label} onSelect={(bucket) => applyDimension('upstreamId', bucket)} /></>}
      {tab === 'heatmap' && <article className="panel"><div className="panel-title"><h2>{t('usage.heatmap')}</h2><span>{stats.time_zone}</span></div><Heatmap values={stats.heatmap} /></article>}
    </section>}
    {!loading && token.trim() && !error && !stats && <div className="empty">{t('usage.noData')}</div>}
  </div>;
}
