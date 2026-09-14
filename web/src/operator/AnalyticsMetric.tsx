import type { ReactNode } from 'react';
import { metricArea } from './analyticsPresentation';
import './analyticsMetrics.css';

export function AnalyticsMetric({ label, labelContent, value, title, tone = '', trend, ratio, note }: {
  label: string; labelContent?: ReactNode; value: ReactNode; title?: string; tone?: string;
  trend?: readonly (number | null)[]; ratio?: number | null; note?: string;
}) {
  const area = trend ? metricArea(trend) : undefined;
  const share = ratio != null && Number.isFinite(ratio) ? Math.max(0, Math.min(1, ratio)) : undefined;
  return <div className={`metric analytics-metric ${tone}`}>
    {area ? <svg className="analytics-metric-trend" viewBox="0 0 200 48" preserveAspectRatio="none" aria-hidden="true" data-samples={trend?.length}><path d={area} /></svg>
      : share !== undefined && <span className="analytics-metric-ratio" aria-hidden="true" data-ratio={share} style={{ width: `${share * 100}%` }} />}
    <span className="metric-label">{labelContent ?? label}</span>
    <strong className="metric-value" title={title}>{value}</strong>
    {note && <span className="analytics-metric-note">{note}</span>}
  </div>;
}
