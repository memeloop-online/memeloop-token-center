import { Slider } from '@fluentui/react-components';
import { useI18n } from '../../i18n';
import { requestRefreshIntervals } from './requestRefresh';
import './RequestRefreshControl.css';

export function RequestRefreshControl({ intervalMs, onIntervalChange, paused, pausedHint, supportsLive = true, refreshing = false }: {
  intervalMs: number;
  onIntervalChange: (value: number) => void;
  paused: boolean;
  pausedHint?: string;
  supportsLive?: boolean;
  refreshing?: boolean;
}) {
  const { t } = useI18n();
  const labels = [
    t(supportsLive === false ? 'requestRefresh.manual' : 'requestRefresh.live'),
    t('requestRefresh.fiveSeconds'),
    t('requestRefresh.thirtySeconds'),
    t('requestRefresh.oneMinute'),
    t('requestRefresh.fiveMinutes'),
  ];
  const index = Math.max(0, requestRefreshIntervals.findIndex(value => value === intervalMs));
  return <div className="request-refresh-control">
    <label>{t('requestRefresh.cadence')} <strong aria-hidden="true">{labels[index]}</strong></label>
    <div className="request-refresh-slider">
      <Slider min={0} max={4} step={1} value={index} aria-label={t('requestRefresh.cadence')} aria-valuetext={labels[index]}
        onChange={(_, data) => onIntervalChange(requestRefreshIntervals[data.value])} />
      <div className="request-refresh-ticks" aria-hidden="true">{labels.map(label => <span key={label}>{label}</span>)}</div>
    </div>
    <span className="request-refresh-hint" role="status">
      {refreshing
        ? t('requestRefresh.refreshing')
        : paused && intervalMs !== 0
        ? (pausedHint ?? t('requestRefresh.paused'))
        : intervalMs === 0
          ? t(supportsLive === false ? 'requestRefresh.manualHint' : 'requestRefresh.liveHint')
          : t('requestRefresh.polling', { interval: labels[index] })}
    </span>
  </div>;
}
