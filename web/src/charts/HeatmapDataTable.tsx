import type { UsageAnalysisHeatmapBucket } from '../types';
import { heatmapValue, type HeatmapMetric, type UsageChartFormatters } from './usageCharts';

export function HeatmapDataTable({
  currency,
  format,
  metric,
  onSelect,
  summary,
  timeZone,
  values,
  valueLabel,
  weekdays,
}: {
  currency: string;
  format: UsageChartFormatters;
  metric: HeatmapMetric;
  onSelect?: (value: UsageAnalysisHeatmapBucket, index: number) => void;
  summary: string;
  timeZone: string;
  values: UsageAnalysisHeatmapBucket[];
  valueLabel: string;
  weekdays: string[];
}) {
  const valueFormat = metric === 'failure_rate'
    ? format.percent
    : metric === 'cost'
      ? (value: number) => format.cost(value, currency)
      : format.number;
  return <details className="usage-chart-table usage-heatmap-table"><summary>{summary}</summary><div className="table-scroll"><table>
    <thead><tr><th scope="col">{timeZone}</th><th scope="col">{valueLabel}</th></tr></thead>
    <tbody>{values.map((value, index) => {
      const day = weekdays[Math.floor(value.hour_of_week / 24)] ?? '';
      const hour = String(value.hour_of_week % 24).padStart(2, '0');
      const label = `${day} ${hour}:00`;
      return <tr key={value.hour_of_week}><th scope="row">{onSelect ? <button type="button" className="table-link" onClick={() => onSelect(value, index)}>{label}</button> : label}</th><td>{valueFormat(heatmapValue(value, metric, currency))}</td></tr>;
    })}</tbody>
  </table></div></details>;
}
