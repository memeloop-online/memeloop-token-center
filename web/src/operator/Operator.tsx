import Form from '@rjsf/core';
import validator from '@rjsf/validator-ajv8';
import { useEffect, useMemo, useState } from 'react';
import type { RJSFSchema } from '@rjsf/utils';
import { api, streamSse } from '../api';
import { RequestTable, Shell } from '../components';
import type { ConfigurationSchemas, PluginManifest, ProviderType, RequestEvent, RequestView, UpstreamAccount } from '../types';

type Tab = 'traffic' | 'upstreams' | 'routes' | 'pricing' | 'keys' | 'oauth' | 'services' | 'plugins';

const tabs: Array<[Tab, string]> = [['traffic', '实时请求'], ['upstreams', '上游账号'], ['routes', '模型路由'], ['pricing', '模型计费'], ['keys', '创建 Key'], ['oauth', 'OAuth'], ['services', '服务凭据'], ['plugins', '插件']];

export function Operator() {
  const [token, setToken] = useState(() => sessionStorage.getItem('mtc-service-token') ?? '');
  const [tab, setTab] = useState<Tab>('traffic');
  const [providers, setProviders] = useState<ProviderType[]>([]);
  const [plugins, setPlugins] = useState<PluginManifest[]>([]);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [schemas, setSchemas] = useState<ConfigurationSchemas>();
  const [error, setError] = useState('');

  async function refresh() {
    if (!token.trim()) return;
    sessionStorage.setItem('mtc-service-token', token.trim());
    setError('');
    try {
      const [nextProviders, nextPlugins, nextUpstreams, nextRequests, nextSchemas] = await Promise.all([
        api<ProviderType[]>('/internal/v1/provider-types', token),
        api<PluginManifest[]>('/internal/v1/plugins', token),
        api<UpstreamAccount[]>('/internal/v1/upstreams', token),
        api<RequestView[]>('/internal/v1/requests?limit=100', token),
        api<ConfigurationSchemas>('/internal/v1/schemas', token),
      ]);
      setProviders(nextProviders);
      setPlugins(nextPlugins);
      setUpstreams(nextUpstreams);
      setRequests(nextRequests);
      setSchemas(nextSchemas);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '管理 API 请求失败');
    }
  }

  useEffect(() => { if (token) void refresh(); }, []);
  useEffect(() => {
    if (!token || tab !== 'traffic') return;
    const controller = new AbortController();
    const connect = async () => {
      while (!controller.signal.aborted) {
        try {
          await streamSse<RequestEvent>(
            '/internal/v1/request-events',
            token,
            controller.signal,
            (event) => setRequests((current) => {
              const previous = current.find((request) => request.request_id === event.request_id);
              const next: RequestView = {
                request_id: event.request_id,
                created_at: previous?.created_at ?? event.event_at,
                protocol: event.protocol,
                model: event.model,
                status_code: event.status_code,
                duration_ms: event.duration_ms,
                input_tokens: event.input_tokens,
                output_tokens: event.output_tokens,
                cost: event.cost,
                error_code: event.error_code,
              };
              return [next, ...current.filter((request) => request.request_id !== event.request_id)]
                .sort((left, right) => right.created_at - left.created_at)
                .slice(0, 100);
            }),
          );
        } catch (reason) {
          if (!controller.signal.aborted) {
            setError(reason instanceof Error ? reason.message : '实时请求流已断开');
          }
        }
        await new Promise((resolve) => window.setTimeout(resolve, 1000));
      }
    };
    void connect();
    return () => controller.abort();
  }, [token, tab]);

  return (
    <Shell operator>
      <header className="hero compact"><div><span className="eyebrow">OPERATOR CONTROL PLANE</span><h1>Token Center</h1><p>上游、OAuth、路由、策略与流量诊断。</p></div><div className="credential"><input type="password" value={token} onChange={(event) => setToken(event.target.value)} placeholder="Service token" /><button onClick={() => void refresh()}>连接</button></div></header>
      <nav className="tabs">{tabs.map(([id, label]) => <button key={id} className={tab === id ? 'active' : ''} onClick={() => setTab(id)}>{label}</button>)}</nav>
      {error && <div className="notice error">{error}</div>}
      {tab === 'traffic' && <article className="panel"><div className="panel-title"><h2>实时请求尾流</h2><span>SSE 亚秒级尾查 · 不载入正文</span></div><RequestTable requests={requests} /></article>}
      {tab === 'upstreams' && <Upstreams token={token} providers={providers} values={upstreams} onCreated={() => void refresh()} />}
      {tab === 'routes' && <RouteForm token={token} upstreams={upstreams} />}
      {tab === 'pricing' && <Pricing token={token} schemas={schemas} />}
      {tab === 'keys' && <KeyForm token={token} schema={schemas?.key_create} />}
      {tab === 'oauth' && <OAuth token={token} providers={providers} onCreated={() => void refresh()} />}
      {tab === 'services' && <ServiceTokenForm token={token} schema={schemas?.service_token} />}
      {tab === 'plugins' && <Plugins values={plugins} />}
    </Shell>
  );
}

function Upstreams({ token, providers, values, onCreated }: { token: string; providers: ProviderType[]; values: UpstreamAccount[]; onCreated: () => void }) {
  const [driver, setDriver] = useState('');
  const provider = providers.find((value) => value.id === driver) ?? providers[0];
  const schema = useMemo<RJSFSchema | undefined>(() => provider ? ({ type: 'object', required: ['name', 'config', 'credential'], properties: { name: { type: 'string', title: '账号名称' }, driver: { type: 'string', default: provider.id, readOnly: true }, config: provider.config_schema, credential: provider.credential_schema } } as RJSFSchema) : undefined, [provider]);
  return <section className="two-column operator-grid"><article className="panel"><h2>账号</h2><div className="account-list">{values.map((value) => <div className="account" key={value.id}><div><b>{value.name}</b><span>{value.driver} · {value.auth_kind}</span></div><span className="pill">gen {value.credential_generation}</span></div>)}</div></article><article className="panel form-panel"><h2>新增上游</h2><label>Provider<select value={provider?.id ?? ''} onChange={(event) => setDriver(event.target.value)}>{providers.map((value) => <option key={value.id} value={value.id}>{value.display_name} · {value.source}</option>)}</select></label>{schema ? <Form key={provider.id} schema={schema} validator={validator} onSubmit={async ({ formData }) => { await api('/internal/v1/upstreams', token, { method: 'POST', body: JSON.stringify(formData) }); onCreated(); }}><button type="submit">创建上游账号</button></Form> : <div className="empty">连接管理 API 后加载 Schema</div>}</article></section>;
}

function Plugins({ values }: { values: PluginManifest[] }) {
  return <article className="panel"><div className="panel-title"><h2>已加载插件</h2><span>Wasmtime Component · fail-closed capabilities</span></div><div className="account-list">{values.length === 0 && <div className="empty">当前未挂载插件</div>}{values.map((value) => <div className="account" key={value.id}><div><b>{value.id}</b><span>v{value.version} · WIT {value.wit_version} · {(value.contributions.providers ?? []).length} provider</span></div><span className="pill">{value.contributions.traffic_policy ? 'traffic policy' : 'provider'}</span></div>)}</div></article>;
}

function RouteForm({ token, upstreams }: { token: string; upstreams: UpstreamAccount[] }) {
  const [form, setForm] = useState({ public_model: '', upstream_account_id: '', upstream_model: '', protocol: 'openai', priority: 0 });
  const [message, setMessage] = useState('');
  return <article className="panel form-panel narrow"><h2>创建模型路由</h2><label>公开模型<input value={form.public_model} onChange={(event) => setForm({ ...form, public_model: event.target.value })} /></label><label>上游账号<select value={form.upstream_account_id} onChange={(event) => setForm({ ...form, upstream_account_id: event.target.value })}><option value="">请选择</option>{upstreams.map((value) => <option key={value.id} value={value.id}>{value.name}</option>)}</select></label><label>上游模型<input value={form.upstream_model} onChange={(event) => setForm({ ...form, upstream_model: event.target.value })} /></label><label>协议<select value={form.protocol} onChange={(event) => setForm({ ...form, protocol: event.target.value })}><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="generation">异步多模态生成</option></select></label><button onClick={async () => { await api('/internal/v1/model-routes', token, { method: 'POST', body: JSON.stringify(form) }); setMessage('路由已创建'); }}>创建路由</button>{message && <div className="notice success">{message}</div>}</article>;
}

function Pricing({ token, schemas }: { token: string; schemas?: ConfigurationSchemas }) {
  const [kind, setKind] = useState<'token' | 'generation'>('token');
  const [model, setModel] = useState('');
  const [currency, setCurrency] = useState('USD');
  const [message, setMessage] = useState('');
  const schema = kind === 'generation' ? schemas?.generation_price : schemas?.model_price;
  return <article className="panel form-panel narrow"><h2>模型计费</h2><p className="muted">文本模型按百万 token 定价；视频、图片和工作流按 JSON Schema 选择秒、任务、图片或百万像素。</p><label>类型<select value={kind} onChange={(event) => setKind(event.target.value as 'token' | 'generation')}><option value="token">Token 模型</option><option value="generation">多模态生成</option></select></label><label>公开模型<input value={model} onChange={(event) => setModel(event.target.value)} /></label><label>币种<input value={currency} onChange={(event) => setCurrency(event.target.value.toUpperCase())} maxLength={3} /></label>{schema ? <Form key={kind} schema={schema as RJSFSchema} validator={validator} onSubmit={async ({ formData }) => { const prefix = kind === 'generation' ? 'generation-prices' : 'prices'; await api(`/internal/v1/${prefix}/${encodeURIComponent(currency)}/${encodeURIComponent(model)}`, token, { method: 'POST', body: JSON.stringify(formData) }); setMessage('价格已保存'); }}><button type="submit">保存价格</button></Form> : <div className="empty">连接管理 API 后加载 Schema</div>}{message && <div className="notice success">{message}</div>}</article>;
}

function KeyForm({ token, schema }: { token: string; schema?: Record<string, unknown> }) {
  const [result, setResult] = useState('');
  return <article className="panel form-panel narrow"><h2>创建下游 Key</h2>{schema ? <Form schema={schema as RJSFSchema} validator={validator} onSubmit={async ({ formData }) => { const created = await api<{ key: string }>('/internal/v1/keys', token, { method: 'POST', body: JSON.stringify(formData) }); setResult(created.key); }}><button type="submit">创建 Key</button></Form> : <div className="empty">连接管理 API 后加载 Schema</div>}{result && <div className="one-time"><b>仅显示一次</b><code>{result}</code></div>}</article>;
}

function ServiceTokenForm({ token, schema }: { token: string; schema?: Record<string, unknown> }) {
  const [result, setResult] = useState('');
  return <article className="panel form-panel narrow"><h2>创建服务凭据</h2><p className="muted">给 memeloop web 等内部调用者分配最小 scope；tenant 绑定后不能跨租户。</p>{schema ? <Form schema={schema as RJSFSchema} validator={validator} onSubmit={async ({ formData }) => { const created = await api<{ token: string }>('/internal/v1/service-tokens', token, { method: 'POST', body: JSON.stringify(formData) }); setResult(created.token); }}><button type="submit">创建服务凭据</button></Form> : <div className="empty">连接管理 API 后加载 Schema</div>}{result && <div className="one-time"><b>仅显示一次</b><code>{result}</code></div>}</article>;
}

function OAuth({ token, providers, onCreated }: { token: string; providers: ProviderType[]; onCreated: () => void }) {
  const adapterProviders = providers.filter((provider) => provider.oauth_adapter);
  const [mode, setMode] = useState<'subscription' | 'cursor-direct' | 'plugin-adapter'>('subscription');
  const [provider, setProvider] = useState<'copilot' | 'cursor'>('copilot');
  const [name, setName] = useState('copilot-primary');
  const [baseUrl, setBaseUrl] = useState('http://cpa-subscription-bridge:8080');
  const [bridgeSecret, setBridgeSecret] = useState('');
  const [session, setSession] = useState<{ login_url: string; session_token: string }>();
  const [message, setMessage] = useState('');
  const startPath = mode === 'subscription' ? '/internal/v1/oauth/subscription-bridge/start' : '/internal/v1/oauth/cursor/start';
  const pollPath = mode === 'subscription' ? '/internal/v1/oauth/subscription-bridge/poll' : '/internal/v1/oauth/cursor/poll';
  if (mode === 'plugin-adapter') {
    return <PluginAdapterOAuth token={token} providers={adapterProviders} onCreated={onCreated} onBack={() => setMode('subscription')} />;
  }
  return <article className="panel form-panel narrow"><h2>OAuth 上游</h2><p className="muted">登录状态与 bridge handle 都经加密保存；Copilot/Cursor 的真实 OAuth 状态可只留在独立 bridge PVC。</p><label>接入方式<select value={mode} onChange={(event) => { const next = event.target.value as 'subscription' | 'cursor-direct' | 'plugin-adapter'; setMode(next); setSession(undefined); setMessage(''); if (next === 'subscription') { setName(`${provider}-primary`); setBaseUrl('http://cpa-subscription-bridge:8080'); } else if (next === 'cursor-direct') { setName('cursor-primary'); setBaseUrl('http://cursor-adapter:8080'); } }}><option value="subscription">CPA Subscription Bridge</option><option value="cursor-direct">Cursor 直接 PKCE</option>{adapterProviders.length > 0 && <option value="plugin-adapter">插件 Provider Adapter</option>}</select></label>{mode === 'subscription' && <label>订阅提供商<select value={provider} onChange={(event) => { const next = event.target.value as 'copilot' | 'cursor'; setProvider(next); setName(`${next}-primary`); setSession(undefined); setMessage(''); }}><option value="copilot">GitHub Copilot</option><option value="cursor">Cursor</option></select></label>}<label>账号名称<input value={name} onChange={(event) => setName(event.target.value)} /></label><label>{mode === 'subscription' ? 'Bridge URL' : 'Provider Adapter URL'}<input value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} /></label>{mode === 'subscription' && <label>Bridge Secret（可选）<input type="password" value={bridgeSecret} onChange={(event) => setBridgeSecret(event.target.value)} /></label>}<div className="button-row"><button onClick={async () => { const body = mode === 'subscription' ? { account_name: name, provider, base_url: baseUrl, ...(bridgeSecret ? { bridge_secret: bridgeSecret } : {}) } : { account_name: name, provider_config: { base_url: baseUrl } }; setSession(await api(startPath, token, { method: 'POST', body: JSON.stringify(body) })); setMessage(''); }}>开始登录</button>{session && <a className="button secondary" href={session.login_url} target="_blank" rel="noreferrer">打开授权页</a>}</div>{session && <button onClick={async () => { const result = await api<UpstreamAccount | { status: string; message?: string }>(pollPath, token, { method: 'POST', body: JSON.stringify({ session_token: session.session_token }) }); if ('id' in result) { setMessage(`账号 ${result.id} 已就绪`); onCreated(); } else setMessage(result.message ?? '仍在等待授权'); }}>检查授权结果</button>}{message && <div className="notice success">{message}</div>}</article>;
}

function PluginAdapterOAuth({ token, providers, onCreated, onBack }: { token: string; providers: ProviderType[]; onCreated: () => void; onBack: () => void }) {
  const [driver, setDriver] = useState(providers[0]?.id ?? '');
  const [name, setName] = useState(() => providers[0] ? `${providers[0].id}-primary` : 'plugin-primary');
  const [session, setSession] = useState<{ login_url: string; session_token: string }>();
  const [message, setMessage] = useState('');
  const provider = providers.find((candidate) => candidate.id === driver) ?? providers[0];
  if (!provider) return <article className="panel form-panel narrow"><h2>插件 OAuth Adapter</h2><div className="empty">当前没有插件贡献 OAuth Adapter。</div><button className="secondary" onClick={onBack}>返回</button></article>;
  return <article className="panel form-panel narrow"><div className="panel-title"><h2>插件 OAuth Adapter</h2><button className="secondary" onClick={onBack}>返回内置接入</button></div><p className="muted">插件声明登录、轮询和刷新端点；账号配置仍由 Provider JSON Schema 渲染，凭据加密并按稳定账号 generation 轮换。</p><label>Provider<select value={provider.id} onChange={(event) => { const next = event.target.value; setDriver(next); setName(`${next}-primary`); setSession(undefined); setMessage(''); }}>{providers.map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.display_name} · {candidate.source}</option>)}</select></label><label>账号名称<input value={name} onChange={(event) => setName(event.target.value)} /></label>{session ? <><div className="button-row"><a className="button secondary" href={session.login_url} target="_blank" rel="noreferrer">打开授权页</a><button onClick={async () => { const result = await api<UpstreamAccount | { status: string; message?: string }>('/internal/v1/oauth/provider-adapter/poll', token, { method: 'POST', body: JSON.stringify({ session_token: session.session_token }) }); if ('id' in result) { setMessage(`账号 ${result.id} 已就绪`); onCreated(); } else setMessage(result.message ?? '仍在等待授权'); }}>检查授权结果</button></div></> : <Form key={provider.id} schema={provider.config_schema as RJSFSchema} validator={validator} onSubmit={async ({ formData }) => { setSession(await api('/internal/v1/oauth/provider-adapter/start', token, { method: 'POST', body: JSON.stringify({ account_name: name, provider_driver: provider.id, provider_config: formData }) })); setMessage(''); }}><button type="submit">开始登录</button></Form>}{message && <div className="notice success">{message}</div>}</article>;
}
