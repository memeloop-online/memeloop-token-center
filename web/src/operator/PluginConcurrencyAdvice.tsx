import { useEffect, useState } from 'react';
import { api } from '../api.js';

const copy = {
  en: {
    title: 'Concurrency observations',
    intro: 'Compare observed account and proxy load with completed requests. Suggested ranges are for manual review; they do not change routing or concurrency.',
    loading: 'Loading observations…',
    select_tenant: 'Choose a tenant to see its observations.',
    not_configured: 'Concurrency observations are not enabled for this tenant. Ask an administrator to connect its private observation service.',
    permission_required: 'Viewing these observations requires access to providers and request history.',
    unavailable: 'Observations are temporarily unavailable. Try refreshing shortly.',
    empty: 'Waiting for observed account or proxy activity.',
    stale: 'These observations are out of date. Wait for fresh samples before changing settings.',
    refresh: 'Refresh', resource: 'Resource', current: 'Current / sampled peak', samples: 'Matched / target samples per bucket',
    progress: 'Sample / observation window progress', range: 'Observed range for manual review', none: 'More comparable evidence is needed',
    account: 'Account', proxy: 'Proxy', detail: 'Evidence details',
    caution: 'Observations describe traffic seen so far, not a proven optimum or the causal effect of increasing concurrency. Different models and request sizes may not be comparable. Confidence intervals rely on independent, comparable samples; repeated inspection and correlated traffic can weaken them.',
    limits: 'Statistical assumptions and limitations', unknown: 'Unknown',
  },
  zh: {
    title: '并发观测',
    intro: '对照账号及代理的实测负载与已完成请求。建议区间仅供人工审阅，不会更改路由或并发配置。',
    loading: '正在读取观测…',
    select_tenant: '选择租户以查看其观测。',
    not_configured: '此租户尚未启用并发观测。请联系管理员连接该租户的私有观测服务。',
    permission_required: '查看这些观测需要供应商与请求历史的读取权限。',
    unavailable: '暂时无法读取观测，请稍后刷新。',
    empty: '正在等待账号或代理的实际请求活动。',
    stale: '观测已过期。请等待新鲜样本后再调整配置。',
    refresh: '刷新', resource: '资源', current: '当前 / 已采样峰值', samples: '已匹配 / 每桶目标样本',
    progress: '样本 / 观察窗进度', range: '供人工审阅的实测区间', none: '仍需更多可比较证据',
    account: '账号', proxy: '代理', detail: '证据详情',
    caution: '观测仅描述已发生的流量，不证明最优并发，也不证明提高并发的因果效果。不同模型和请求长度可能无法比较。置信区间依赖独立、可比较样本；反复查看及相关流量可能削弱其有效性。',
    limits: '统计假设与局限', unknown: '未知',
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
    setResult(undefined);
    const query = tenant ? `?tenant_external_id=${encodeURIComponent(tenant)}` : '';
    void api<unknown>(`/internal/v1/plugins/mtc-health-intelligence/private-data/concurrency-advice${query}`, token)
      .then((value) => { if (active) setResult({ token, tenant, value: object(value) ?? { state: 'unavailable' } }); })
      .catch(() => { if (active) setResult({ token, tenant, value: { state: 'unavailable' } }); });
    return () => { active = false; };
  }, [token, tenant, refresh]);

  const data = object(response?.data);
  const valid = response?.state === 'ready' && data?.contract_version === 'health_concurrency_advice_v1'
    && data.application_mode === 'manual_only' && typeof data.stale === 'boolean' && Array.isArray(data.series)
    && (!tenant || data.tenant_external_id === tenant);
  const state = response?.state;
  const message = !response ? text.loading : state === 'select_tenant' ? text.select_tenant
    : state === 'not_configured' ? text.not_configured : state === 'permission_required' ? text.permission_required
      : !valid ? text.unavailable : '';
  const series = valid && Array.isArray(data?.series) ? data.series.map(object).filter((s): s is RecordValue => s !== null) : [];
  const uncertainty = object(data?.uncertainty);
  return <section className="panel" aria-label={text.title}>
    <div className="panel-title"><div><h2>{text.title}</h2><p className="muted">{text.intro}</p></div>
      <button type="button" disabled={!response} onClick={() => setRefresh((v) => v + 1)}>{text.refresh}</button>
    </div>
    {message ? <p role="status">{message}</p> : <>
      {data?.stale === true && <p className="notice" role="status">{text.stale}</p>}
      {series.length === 0 && <p>{text.empty}</p>}
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
