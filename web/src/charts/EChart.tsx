import { useEffect, useRef, useState } from 'react';
import { init, use, type EChartsType } from 'echarts/core';
import type { EChartsCoreOption } from 'echarts/types/dist/core';
import { BarChart, HeatmapChart, LineChart } from 'echarts/charts';
import {
  AriaComponent,
  DataZoomComponent,
  GridComponent,
  LegendComponent,
  TooltipComponent,
  VisualMapComponent,
} from 'echarts/components';
import { CanvasRenderer } from 'echarts/renderers';

use([
  LineChart,
  BarChart,
  HeatmapChart,
  GridComponent,
  TooltipComponent,
  LegendComponent,
  DataZoomComponent,
  VisualMapComponent,
  AriaComponent,
  CanvasRenderer,
]);

export interface EChartClick {
  dataIndex: number;
  seriesIndex?: number;
  value: unknown;
}

interface EChartProps {
  ariaLabel: string;
  className?: string;
  locale: 'zh-CN' | 'en';
  onClick?: (event: EChartClick) => void;
  option: EChartsCoreOption;
  timeZone: string;
}

const darkTheme = {
  color: ['#68dec9', '#ff9c72', '#82aaff', '#d7a9ff', '#ffd166', '#80cbc4'],
  backgroundColor: 'transparent',
  textStyle: { color: '#a6bac0' },
  legend: { textStyle: { color: '#a6bac0' } },
  categoryAxis: { axisLine: { lineStyle: { color: '#30474e' } }, axisLabel: { color: '#82979e' }, splitLine: { lineStyle: { color: '#1d3036' } } },
  valueAxis: { axisLine: { lineStyle: { color: '#30474e' } }, axisLabel: { color: '#82979e' }, splitLine: { lineStyle: { color: '#1d3036' } } },
};

function currentTheme() {
  return document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
}

export function EChart({ ariaLabel, className = '', locale, onClick, option, timeZone }: EChartProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<EChartsType | null>(null);
  const clickRef = useRef(onClick);
  const optionRef = useRef(option);
  const [theme, setTheme] = useState(currentTheme);
  clickRef.current = onClick;
  optionRef.current = option;

  useEffect(() => {
    const observer = new MutationObserver(() => setTheme(currentTheme()));
    observer.observe(document.documentElement, { attributeFilter: ['data-theme'] });
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const chart = init(host, theme === 'dark' ? darkTheme : undefined, {
      locale: locale === 'zh-CN' ? 'ZH' : 'EN',
      renderer: 'canvas',
    });
    chartRef.current = chart;
    chart.setOption(optionRef.current, { notMerge: true, lazyUpdate: true });
    chart.on('click', (event) => clickRef.current?.({
      dataIndex: event.dataIndex,
      seriesIndex: event.seriesIndex,
      value: event.value,
    }));
    const resizeObserver = new ResizeObserver(() => chart.resize());
    resizeObserver.observe(host);
    return () => {
      resizeObserver.disconnect();
      chart.dispose();
      if (chartRef.current === chart) chartRef.current = null;
    };
  }, [locale, theme]);

  useEffect(() => {
    chartRef.current?.setOption(option, { notMerge: true, lazyUpdate: true });
  }, [option]);

  return <div
    aria-label={`${ariaLabel} (${timeZone})`}
    className={`usage-echart ${className}`.trim()}
    ref={hostRef}
    role="img"
  />;
}
