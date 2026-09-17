import { useId } from 'react';
import { Slider } from '@fluentui/react-components';
import { useI18n } from '../../i18n';
import { requestRefreshIntervals } from './requestRefresh';
import './RequestRefreshControl.css';

export function RequestRefreshControl({ intervalMs, onIntervalChange, paused, pausedHint }: {
  intervalMs: number; onIntervalChange: (value: number) => void; paused: boolean; pausedHint?: string;
}) {
  const { locale } = useI18n();
  const id = useId();
  const zh = locale === 'zh-CN';
  const labels = zh ? ['实时', '5秒', '30秒', '1分', '5分'] : ['Live', '5s', '30s', '1m', '5m'];
  const index = Math.max(0, requestRefreshIntervals.findIndex(value => value === intervalMs));
  return <div className="request-refresh-control">
    <label id={id}>{zh ? '刷新节奏' : 'Refresh cadence'} <strong>{labels[index]}</strong></label>
    <div className="request-refresh-slider">
      <Slider min={0} max={4} step={1} value={index} aria-labelledby={id} aria-valuetext={labels[index]}
        onChange={(_, data) => onIntervalChange(requestRefreshIntervals[data.value])} />
      <div className="request-refresh-ticks" aria-hidden="true">{labels.map(label => <span key={label}>{label}</span>)}</div>
    </div>
    <span className="request-refresh-hint">{paused ? (pausedHint ?? (zh ? '后台已暂停，返回后继续' : 'Paused in background; resumes on return')) : (zh ? '列表与统计同步更新' : 'List and summary update together')}</span>
  </div>;
}
