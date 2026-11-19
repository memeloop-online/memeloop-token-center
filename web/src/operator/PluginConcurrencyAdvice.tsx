import { useEffect, useState } from 'react';
import { api } from '../api.js';

const copy = {
  en: {
    title: 'Concurrency observations',
    intro: 'View account and proxy request load, completed requests, and suggested concurrency ranges.',
    unavailable: 'Observations are temporarily unavailable. Try refreshing shortly.',
    stale: 'Showing the previous observations. Refresh to check for new samples.',
    refresh: 'Refresh', resource: 'Resource', current: 'Current / sampled peak', samples: 'Matched / target samples per bucket',
    progress: 'Sample / observation window progress', range: 'Suggested concurrency range', none: 'Waiting for enough samples',
    account: 'Account', proxy: 'Proxy', detail: 'Evidence details',
    caution: 'Ranges describe the current sample. Compare model, request length, and sampling coverage together. Confidence intervals assume independent, comparable samples; repeated observations and correlated traffic affect confidence.',
    limits: 'How ranges are calculated', unknown: 'Unknown',
  },
  zh: {
    title: '并发观测',
    intro: '查看账号与代理的请求负载、完成量和建议并发区间。',
    unavailable: '暂时无法读取观测，请稍后刷新。',
    stale: '当前显示上次观测结果，可刷新查看新样本。',
    refresh: '刷新', resource: '资源', current: '当前 / 已采样峰值', samples: '已匹配 / 每桶目标样本',
    progress: '样本 / 观察窗进度', range: '建议并发区间', none: '等待足够样本',
    account: '账号', proxy: '代理', detail: '证据详情',
    caution: '建议区间基于当前样本，请结合模型、请求长度与采样覆盖比较。置信区间以独立、可比较样本为前提；重复观测与相关流量会影响可信度。',
    limits: '区间计算依据', unknown: '未知',
  },
};

type RecordValue = Record<string, unknown>;
function object(value: unknown): RecordValue | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? value as RecordValue : null;
}
function number(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;
}
function percent(value: unknown): string {
  const n = number(value);
  return n === null ? '—' : `${Math.round(Math.min(1, n) * 100)}%`;
}
function strings(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((s): s is string => typeof s === 'string') : [];
}

export function PluginConcurrencyAdvice({ token, tenant, locale }: { token: string; tenant: string; locale: string }) {
  const text = locale.startsWith('zh') ? copy.zh : copy.en;
  const [result, setResult] = useState<{ token: string; tenant: string; value: RecordValue }>();
  const response = result?.token === token && result?.tenant === tenant ? result.value : undefined;
  const [refresh, setRefresh] = useState(0);
  useEffect(() => {
    let active = true;
    const controller = new AbortController();
    setResult(undefined);
    const query = tenant ? `?tenant_external_id=${encodeURIComponent(tenant)}` : '';
    void api<unknown>(`/internal/v1/plugins/mtc-health-intelligence/private-data/concurrency-advice${query}`, token, {
      signal: controller.signal, cache: 'no-store',
    })
      .then((value) => { if (active) setResult({ token, tenant, value: object(value) ?? { state: 'unavailable' } }); })
      .catch(() => { if (active) setResult({ token, tenant, value: { state: 'unavailable' } }); });
    return () => { active = false; controller.abort(); };
  }, [token, tenant, refresh]);

  const data = object(response?.data);
  const valid = response?.state === 'ready' && data?.contract_version === 'health_concurrency_advice_v1'
    && data.application_mode === 'manual_only' && typeof data.stale === 'boolean' && Array.isArray(data.series)
    && (!tenant || data.tenant_external_id === tenant);
  const state = response?.state;
  if (!response || state === 'select_tenant' || state === 'not_configured' || state === 'permission_required'
    || (valid && Array.isArray(data?.series) && data.series.length === 0)) return null;
  const message = valid ? '' : text.unavailable;
  const series = valid && Array.isArray(data?.series) ? data.series.map(object).filter((s): s is RecordValue => s !== null) : [];
  const uncertainty = object(data?.uncertainty);
  return <section className="panel" aria-label={text.title}>
    <div className="panel-title"><div><h2>{text.title}</h2>{valid && <p className="muted">{text.intro}</p>}</div>
      <button type="button" disabled={!response} onClick={() => setRefresh((v) => v + 1)}>{text.refresh}</button>
    </div>
    {message ? <p role="status">{message}</p> : <>
      {data?.stale === true && <p className="notice" role="status">{text.stale}</p>}
      {series.map((item, index) => {
        const sampling = object(item.sampling);
        const recommendation = data?.stale === false && item.status === 'manual_review' ? object(item.recommendation) : null;
        const range = recommendation?.kind === 'observed_range_for_manual_review' ? object(recommendation.concurrencyRange) : null;
        const from = number(range?.from); const to = number(range?.to);
        return <section className="managed-resource" key={`${String(item.dimension)}:${String(item.identity)}:${index}`}>
          <h3>{item.dimension === 'network_proxy' ? text.proxy : text.account} · {typeof item.identity === 'string' ? item.identity : text.unknown}</h3>
          <dl>
            <dt>{text.current}</dt><dd>{number(item.current) ?? '—'} / {number(item.sampled_peak) ?? '—'}</dd>
            <dt>{text.samples}</dt><dd>{number(sampling?.largestBucketSamples) ?? '—'} / {number(sampling?.targetSamplesPerBucket) ?? '—'}</dd>
            <dt>{text.progress}</dt><dd>{percent(sampling?.sampleProgressInBestBucket)} / {percent(sampling?.windowProgressInBestBucket)}</dd>
            <dt>{text.range}</dt><dd>{from !== null && to !== null && to >= from ? `${from}–${to}` : text.none}</dd>
          </dl>
          <details><summary>{text.detail}</summary><pre className="plugin-typed-data">{JSON.stringify({
            measurement_basis: item.measurement_basis, trend: item.trend, sampling: item.sampling,
            coverage: item.coverage, buckets: item.buckets, recommendation: data?.stale === false ? item.recommendation : null,
          }, null, 2)}</pre></details>
        </section>;
      })}
      <p className="notice">{text.caution}</p>
      <details><summary>{text.limits}</summary>
        <ul>{[...strings(uncertainty?.assumptions), ...strings(data?.limitations)].map((value, i) => <li key={i}>{value}</li>)}</ul>
        <pre className="plugin-typed-data">{JSON.stringify({ uncertainty, policy: data?.policy, observation: data?.observation, collection: data?.collection }, null, 2)}</pre>
      </details>
    </>}
  </section>;
}
