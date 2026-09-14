import { useId, useState, type ReactNode } from 'react';
import { Tab, TabList } from '../design-system';
import { useI18n } from '../i18n';
import './chartDataView.css';

/** Presentation-only switch: both views keep the same data and mounted chart. */
export function ChartDataView({ title, metadata, children, data }: {
  title: string; metadata?: ReactNode; children: ReactNode; data: ReactNode;
}) {
  const { t } = useI18n();
  const id = useId();
  const [view, setView] = useState('chart');
  return <>
    <div className="panel-title chart-view-title"><h2>{title}</h2><div className="chart-view-controls">{metadata}
      <TabList size="small" selectedValue={view} onTabSelect={(_, value) => setView(String(value.value))} aria-label={title}>
        <Tab id={`${id}-chart-tab`} aria-controls={`${id}-chart`} value="chart">{t('usage.chartView')}</Tab>
        <Tab id={`${id}-data-tab`} aria-controls={`${id}-data`} value="data">{t('usage.dataView')}</Tab>
      </TabList>
    </div></div>
    <div role="tabpanel" tabIndex={0} id={`${id}-chart`} aria-labelledby={`${id}-chart-tab`} hidden={view !== 'chart'}>{children}</div>
    <div role="tabpanel" tabIndex={0} id={`${id}-data`} aria-labelledby={`${id}-data-tab`} hidden={view !== 'data'}>{data}</div>
  </>;
}
