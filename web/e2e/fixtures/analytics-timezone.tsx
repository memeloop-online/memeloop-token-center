import { createRoot } from 'react-dom/client';
import { getInstanceByDom } from 'echarts/core';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { UsageAnalysis } from '../../src/operator/UsageAnalysis';
import { statsQuery } from '../../src/operator/usageState';
import '../../src/styles.css';
const hourBucket = 497103;
const epoch = hourBucket * 3_600_000;
const metrics = { requests: 3, success: 2, failed: 1, input_tokens: 10, output_tokens: 2, cached_input_tokens: 0, cache_write_tokens: 0, generation_units: 0, avg_duration_ms: 100, p95_duration_ms: 200, p95_is_capped: false, costs: [] };
const stats = { summary: metrics, time_zone: 'UTC', from_created_at: epoch, to_created_at: epoch + 7_200_000, granularity: 'hour', time_series: [{ ...metrics, bucket_start: epoch }, { ...metrics, bucket_start: epoch + 3_600_000 }], by_model: [], by_key: [], by_session: [], by_upstream: [], by_protocol: [], by_status: [], errors: [], heatmap: [{ ...metrics, hour_of_week: 12 }] };
let reads = 0;
globalThis.fetch = async () => { reads++; return new Response(JSON.stringify(stats), { headers: { 'Content-Type': 'application/json' } }); };
Object.assign(window, { timeFixture: {
  epoch,
  reads: () => reads,
  chartLabels: () => { const host = document.querySelector<HTMLElement>('.usage-echart'); return host ? (getInstanceByDom(host)?.getOption().xAxis as Array<{ data: string[] }> | undefined)?.[0]?.data : undefined; },
  chartTooltip: () => {
    const host = document.querySelector<HTMLElement>('.usage-echart');
    const formatter = host ? (getInstanceByDom(host)?.getOption().tooltip as Array<{ formatter?: unknown }> | undefined)?.[0]?.formatter : undefined;
    return typeof formatter === 'function' ? formatter([{ dataIndex: 0, marker: '', seriesName: 'Successful', value: 2 }]) : undefined;
  },
  todayQuery: () => statsQuery('', { preset: 'today', granularity: 'hour', customFrom: '', customTo: '', filters: { model: '', keyId: '', upstreamId: '', protocol: '', status: '', errorCode: '' } }),
} });
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><UsageAnalysis token="fixture" tenant="fixture" upstreams={[]} onOpenSession={() => undefined}/></MtcFluentProvider></I18nProvider>);
