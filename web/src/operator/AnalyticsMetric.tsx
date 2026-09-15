import { useState, type ReactNode } from 'react';
import { Tooltip } from '@fluentui/react-components';
import { useI18n } from '../i18n.js';
import { formatNumber } from '../format.js';
import { metricArea } from './analyticsPresentation.js';
import './analyticsMetrics.css';

export function AnalyticsMetric({ label, labelContent, value, title, tone = '', trend, ratio, note, timestamps, timeZone, formatSample }: {
  label: string; labelContent?: ReactNode; value: ReactNode; title?: string; tone?: string;
  trend?: readonly (number | null)[]; ratio?: number | null; note?: string;
  timestamps?: readonly number[]; timeZone?: string; formatSample?: (value: number | null, index: number) => string;
}) {
  const { locale } = useI18n();
  const [index, setIndex] = useState<number>();
  const interactive = Boolean(trend?.length && timestamps?.length === trend.length);
  const selected = index === undefined ? 0 : Math.min(index, (trend?.length ?? 1) - 1);
  const sample = trend?.[selected];
  const sampleText = interactive && formatSample ? formatSample(sample ?? null, selected) : sample == null || !Number.isFinite(sample) ? '—' : formatNumber(sample, locale, 6);
  const time = timestamps?.[selected];
  const detail = `${time === undefined ? '' : new Date(time).toLocaleString(locale, { timeZone })}${timeZone ? ` ${timeZone}` : ''} · ${label}: ${sampleText}`;
  const area = trend ? metricArea(trend) : undefined;
  const share = ratio != null && Number.isFinite(ratio) ? Math.max(0, Math.min(1, ratio)) : undefined;
  // Supplemental label details own their focus/tap; do not also activate the trend.
  const isLabelDetail = (target: EventTarget) => labelContent !== undefined && target instanceof Element && Boolean(target.closest('.metric-label'));
  const card = <div className={`metric analytics-metric ${tone}`} tabIndex={interactive ? 0 : undefined}
    role={interactive ? 'slider' : undefined} aria-label={interactive ? label : undefined}
    aria-valuemin={interactive ? 1 : undefined} aria-valuemax={interactive ? trend!.length : undefined} aria-valuenow={interactive ? selected + 1 : undefined} aria-valuetext={interactive ? detail : undefined}
    onFocus={event => { if (interactive) setIndex(event.target === event.currentTarget ? 0 : undefined); }} onBlur={() => setIndex(undefined)}
    onPointerMove={event => { if (isLabelDetail(event.target)) { setIndex(undefined); return; } if (interactive) { const bounds = event.currentTarget.getBoundingClientRect(); setIndex(Math.max(0, Math.min(trend!.length - 1, Math.round((event.clientX - bounds.left) / bounds.width * (trend!.length - 1))))); } }}
    onPointerDown={event => { if (isLabelDetail(event.target)) { setIndex(undefined); return; } if (interactive) { event.currentTarget.focus({ preventScroll: true }); const bounds = event.currentTarget.getBoundingClientRect(); setIndex(Math.max(0, Math.min(trend!.length - 1, Math.round((event.clientX - bounds.left) / bounds.width * (trend!.length - 1))))); } }}
    onPointerLeave={event => { if (event.pointerType !== 'touch' && document.activeElement !== event.currentTarget) setIndex(undefined); }}
    onKeyDown={event => { if (!interactive || event.target !== event.currentTarget) return; if (event.key === 'Escape') { setIndex(undefined); return; } const next = event.key === 'Home' ? 0 : event.key === 'End' ? trend!.length - 1 : event.key === 'ArrowRight' || event.key === 'ArrowUp' ? selected + 1 : event.key === 'ArrowLeft' || event.key === 'ArrowDown' ? selected - 1 : undefined; if (next !== undefined) { event.preventDefault(); setIndex(Math.max(0, Math.min(trend!.length - 1, next))); } }}>
    {area ? <svg className="analytics-metric-trend" viewBox="0 0 200 48" preserveAspectRatio="none" aria-hidden="true" data-samples={trend?.length}><path d={area} /></svg>
      : share !== undefined && <span className="analytics-metric-ratio" aria-hidden="true" data-ratio={share} style={{ width: `${share * 100}%` }} />}
    <span className="metric-label">{labelContent ?? label}</span>
    <strong className="metric-value" title={title}>{value}</strong>
    {note && <span className="analytics-metric-note">{note}</span>}
    {interactive && index !== undefined && <span className="analytics-metric-cursor" aria-hidden="true" style={{ left: `${selected / Math.max(1, trend!.length - 1) * 100}%` }} />}
  </div>;
  return interactive ? <Tooltip content={detail} relationship="description" visible={index !== undefined} positioning="above" withArrow>{card}</Tooltip> : card;
}
