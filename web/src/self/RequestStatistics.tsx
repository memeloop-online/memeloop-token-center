import { Buckets, NumberMetric } from '../components';
import { formatMetricDisplay, formatNumber } from '../format';
import { useI18n } from '../i18n';
import { AnalyticsMetric } from '../operator/AnalyticsMetric';
import type { SelfStats } from '../types';
import { cacheStatisticsCopy, cacheStatisticsPresentation } from './cacheStatistics';

export function RequestStatistics({ stats, onModelSelect }: {
  stats: SelfStats;
  onModelSelect: (model: string) => void;
}) {
  const { locale, t } = useI18n();
  const copy = cacheStatisticsCopy[locale];
  const cache = cacheStatisticsPresentation(stats.summary.cache_usage);
  const days = stats.by_day.filter(day => Number.isFinite(Date.parse(`${day.name}T00:00:00Z`)));
  const timestamps = days.map(day => Date.parse(`${day.name}T00:00:00Z`));
  const countMetric = (label: string, value: number, trend: number[]) => {
    const display = formatMetricDisplay(value, locale);
    return <AnalyticsMetric label={label} value={<span className="metric-number"><span className="metric-exact" title={display.title ?? display.text}>{display.text}</span></span>}
      trend={trend} timestamps={timestamps} timeZone="UTC" formatSample={sample => formatNumber(sample ?? 0, locale)} />;
  };
  return <>
    <section className="metrics self-request-summary">
      {countMetric(t('traffic.total'), stats.summary.total_requests, days.map(day => day.requests))}
      <NumberMetric label={t('traffic.success')} value={stats.summary.successful_requests} tone="positive" />
      <NumberMetric label={t('traffic.failure')} value={stats.summary.failed_requests} tone="negative" />
      {countMetric(t('request.tokens'), stats.summary.input_tokens + stats.summary.output_tokens, days.map(day => day.input_tokens + day.output_tokens))}
      <AnalyticsMetric label={copy.tokens} value={cache.tokens === null ? copy.unknown : formatNumber(cache.tokens, locale)} note={copy[cache.note]} />
      <AnalyticsMetric label={copy.rate} value={cache.rate === null ? copy.unknown : new Intl.NumberFormat(locale, { style: 'percent', maximumFractionDigits: 2 }).format(cache.rate)} ratio={cache.rate} note={copy[cache.note]} />
    </section>
    <section className="two-column self-request-breakdown">
      <article className="panel"><h2>{t('traffic.models')}</h2><Buckets values={stats.by_model} onSelect={bucket => onModelSelect(bucket.name)} /></article>
      <article className="panel"><h2>{t('traffic.days')}</h2><Buckets values={stats.by_day} /></article>
    </section>
  </>;
}
