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
import { dataThemes } from '../design-system/dataTheme';

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

function chartTheme(mode: 'light' | 'dark') {
  const colors = dataThemes[mode];
  return {
  color: [colors.primary, colors.negative, colors.secondary, '#9279a6', '#a98432', '#548c81'],
  backgroundColor: 'transparent',
  textStyle: { color: colors.muted, fontSize: 12 },
  legend: { textStyle: { color: colors.muted, fontSize: 12 } },
  categoryAxis: { axisLine: { lineStyle: { color: colors.border } }, axisLabel: { color: colors.muted }, splitLine: { lineStyle: { color: colors.border } } },
  valueAxis: { axisLine: { lineStyle: { color: colors.border } }, axisLabel: { color: colors.muted }, splitLine: { lineStyle: { color: colors.border } } },
  };
}

function currentTheme(): 'light' | 'dark' {
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
    const chart = init(host, chartTheme(theme), {
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
