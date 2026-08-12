import Form from '@rjsf/core';
import validator from '@rjsf/validator-ajv8';
import { useEffect, useMemo, useState } from 'react';
import type { RJSFSchema } from '@rjsf/utils';
import { api } from '../api';
import { RequestTable, Shell } from '../components';
import type { ProviderType, RequestView, UpstreamAccount } from '../types';

type Tab = 'traffic' | 'upstreams' | 'routes' | 'keys' | 'oauth';

const tabs: Array<[Tab, string]> = [['traffic', '实时请求'], ['upstreams', '上游账号'], ['routes', '模型路由'], ['keys', '创建 Key'], ['oauth', 'OAuth']];

export function Operator() {
  const [token, setToken] = useState(() => sessionStorage.getItem('mtc-service-token') ?? '');
  const [tab, setTab] = useState<Tab>('traffic');
  const [providers, setProviders] = useState<ProviderType[]>([]);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [error, setError] = useState('');
  const provider = providers[0];

  async function refresh() {
    if (!token.trim()) return;
    sessionStorage.setItem('mtc-service-token', token.trim());
    setError('');
    try {
      const [nextProviders, nextUpstreams, nextRequests] = await Promise.all([
        api<ProviderType[]>('/internal/v1/provider-types', token),
        api<UpstreamAccount[]>('/internal/v1/upstreams', token),
        api<RequestView[]>('/internal/v1/requests?limit=100', token),
      ]);
      setProviders(nextProviders);
      setUpstreams(nextUpstreams);
      setRequests(nextRequests);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '管理 API 请求失败');
    }
  }

  useEffect(() => { if (token) void refresh(); }, []);
  useEffect(() => {
    if (!token || tab !== 'traffic') return;
    const timer = window.setInterval(() => void refresh(), 3000);
    return () => window.clearInterval(timer);
  }, [token, tab]);

  return (
    <Shell operator>
      <header className="hero compact"><div><span className="eyebrow">OPERATOR CONTROL PLANE</span><h1>Token Center</h1><p>上游、OAuth、路由、策略与流量诊断。</p></div><div className="credential"><input type="password" value={token} onChange={(event) => setToken(event.target.value)} placeholder="Service token" /><button onClick={() => void refresh()}>连接</button></div></header>
      <nav className="tabs">{tabs.map(([id, label]) => <button key={id} className={tab === id ? 'active' : ''} onClick={() => setTab(id)}>{label}</button>)}</nav>
      {error && <div className="notice error">{error}</div>}
      {tab === 'traffic' && <article className="panel"><div className="panel-title"><h2>实时请求尾流</h2><span>每 3 秒刷新 · 不载入正文</span></div><RequestTable requests={requests} /></article>}
      {tab === 'upstreams' && <Upstreams token={token} provider={provider} values={upstreams} onCreated={() => void refresh()} />}
      {tab === 'routes' && <RouteForm token={token} upstreams={upstreams} />}
      {tab === 'keys' && <KeyForm token={token} />}
      {tab === 'oauth' && <OAuth token={token} onCreated={() => void refresh()} />}
    </Shell>
  );
}

function Upstreams({ token, provider, values, onCreated }: { token: string; provider?: ProviderType; values: UpstreamAccount[]; onCreated: () => void }) {
  const schema = useMemo<RJSFSchema | undefined>(() => provider ? ({ type: 'object', required: ['name', 'config', 'credential'], properties: { name: { type: 'string', title: '账号名称' }, driver: { type: 'string', default: provider.id, readOnly: true }, config: provider.config_schema, credential: provider.credential_schema } } as RJSFSchema) : undefined, [provider]);
  return <section className="two-column operator-grid"><article className="panel"><h2>账号</h2><div className="account-list">{values.map((value) => <div className="account" key={value.id}><div><b>{value.name}</b><span>{value.driver} · {value.auth_kind}</span></div><span className="pill">gen {value.credential_generation}</span></div>)}</div></article><article className="panel form-panel"><h2>新增上游</h2>{schema ? <Form schema={schema} validator={validator} onSubmit={async ({ formData }) => { await api('/internal/v1/upstreams', token, { method: 'POST', body: JSON.stringify(formData) }); onCreated(); }}><button type="submit">创建上游账号</button></Form> : <div className="empty">连接管理 API 后加载 Schema</div>}</article></section>;
}

function RouteForm({ token, upstreams }: { token: string; upstreams: UpstreamAccount[] }) {
  const [form, setForm] = useState({ public_model: '', upstream_account_id: '', upstream_model: '', protocol: 'openai', priority: 0 });
  const [message, setMessage] = useState('');
  return <article className="panel form-panel narrow"><h2>创建模型路由</h2><label>公开模型<input value={form.public_model} onChange={(event) => setForm({ ...form, public_model: event.target.value })} /></label><label>上游账号<select value={form.upstream_account_id} onChange={(event) => setForm({ ...form, upstream_account_id: event.target.value })}><option value="">请选择</option>{upstreams.map((value) => <option key={value.id} value={value.id}>{value.name}</option>)}</select></label><label>上游模型<input value={form.upstream_model} onChange={(event) => setForm({ ...form, upstream_model: event.target.value })} /></label><label>协议<select value={form.protocol} onChange={(event) => setForm({ ...form, protocol: event.target.value })}><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option></select></label><button onClick={async () => { await api('/internal/v1/model-routes', token, { method: 'POST', body: JSON.stringify(form) }); setMessage('路由已创建'); }}>创建路由</button>{message && <div className="notice success">{message}</div>}</article>;
}

function KeyForm({ token }: { token: string }) {
  const [result, setResult] = useState('');
  const schema: RJSFSchema = { type: 'object', required: ['principal_external_id', 'alias', 'currency'], properties: { tenant_external_id: { type: 'string', default: 'default', title: 'Tenant' }, principal_external_id: { type: 'string', title: 'Principal' }, alias: { type: 'string', title: 'Key 别名' }, currency: { type: 'string', enum: ['USD', 'CNY'], default: 'USD' }, initial_balance: { type: 'string', default: '0', title: '初始额度' }, policy: { type: 'object', properties: { allowed_models: { type: 'array', items: { type: 'string' }, title: '允许模型' }, requests_per_minute: { type: 'integer', default: 60 }, tokens_per_minute: { type: 'integer', default: 100000 }, max_concurrency: { type: 'integer', default: 4 } } } } };
  return <article className="panel form-panel narrow"><h2>创建下游 Key</h2><Form schema={schema} validator={validator} onSubmit={async ({ formData }) => { const created = await api<{ key: string }>('/internal/v1/keys', token, { method: 'POST', body: JSON.stringify(formData) }); setResult(created.key); }}><button type="submit">创建 Key</button></Form>{result && <div className="one-time"><b>仅显示一次</b><code>{result}</code></div>}</article>;
}

function OAuth({ token, onCreated }: { token: string; onCreated: () => void }) {
  const [name, setName] = useState('cursor-primary');
  const [baseUrl, setBaseUrl] = useState('http://cursor-adapter:8080');
  const [session, setSession] = useState<{ login_url: string; session_token: string }>();
  const [message, setMessage] = useState('');
  return <article className="panel form-panel narrow"><h2>Cursor OAuth</h2><p className="muted">PKCE 状态经过加密，可在任意 control 副本继续轮询。</p><label>账号名称<input value={name} onChange={(event) => setName(event.target.value)} /></label><label>Provider Adapter URL<input value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} /></label><div className="button-row"><button onClick={async () => setSession(await api('/internal/v1/oauth/cursor/start', token, { method: 'POST', body: JSON.stringify({ account_name: name, provider_config: { base_url: baseUrl } }) }))}>开始登录</button>{session && <a className="button secondary" href={session.login_url} target="_blank" rel="noreferrer">打开授权页</a>}</div>{session && <button onClick={async () => { const result = await api<UpstreamAccount | { status: string }>('/internal/v1/oauth/cursor/poll', token, { method: 'POST', body: JSON.stringify({ session_token: session.session_token }) }); if ('id' in result) { setMessage(`账号 ${result.id} 已就绪`); onCreated(); } else setMessage('仍在等待授权'); }}>检查授权结果</button>}{message && <div className="notice success">{message}</div>}</article>;
}
