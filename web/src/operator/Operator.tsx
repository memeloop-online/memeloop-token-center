import Form from '@rjsf/core';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useMemo, useState } from 'react';
import { api, streamSse } from '../api';
import { Buckets, Metric, RequestTable, Shell } from '../components';
import { localizeSchema, useI18n } from '../i18n';
import { safeValidator as validator } from '../safeValidator';
import type {
  ConfigurationSchemas, ModelPriceSyncResult, ModelPriceUsageSummary, ModelPriceView,
  OperatorStats, PluginManifest, ProviderType, RequestDetail, RequestEvent, RequestView,
  TenantView, UpstreamAccount,
} from '../types';
import './operator.css';

type Tab = 'traffic' | 'providers' | 'routes' | 'pricing' | 'credentials' | 'services' | 'plugins';
const tabIds: Tab[] = ['traffic', 'providers', 'routes', 'pricing', 'credentials', 'services', 'plugins'];

function queryForTenant(tenant: string, existing = '') {
  const params = new URLSearchParams(existing);
  if (tenant) params.set('tenant_external_id', tenant);
  const query = params.toString();
  return query ? `?${query}` : '';
}

export function Operator() {
  const { t } = useI18n();
  const [token, setToken] = useState(() => sessionStorage.getItem('mtc-service-token') ?? '');
  const [tab, setTab] = useState<Tab>('traffic');
  const [tenant, setTenant] = useState('');
  const [tenants, setTenants] = useState<TenantView[]>([]);
  const [providers, setProviders] = useState<ProviderType[]>([]);
  const [plugins, setPlugins] = useState<PluginManifest[]>([]);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [stats, setStats] = useState<OperatorStats>();
  const [detail, setDetail] = useState<RequestDetail>();
  const [schemas, setSchemas] = useState<ConfigurationSchemas>();
  const [error, setError] = useState('');

  async function refresh() {
    const credential = token.trim();
    if (!credential) return;
    sessionStorage.setItem('mtc-service-token', credential);
    setError('');
    const scope = queryForTenant(tenant);
    try {
      const results = await Promise.allSettled([
        api<TenantView[]>('/internal/v1/tenants', credential),
        api<ProviderType[]>('/internal/v1/provider-types', credential),
        api<PluginManifest[]>('/internal/v1/plugins', credential),
        api<UpstreamAccount[]>(`/internal/v1/upstreams${scope}`, credential),
        api<RequestView[]>(`/internal/v1/requests${queryForTenant(tenant, 'limit=100')}`, credential),
        api<OperatorStats>(`/internal/v1/stats${scope}`, credential),
        api<ConfigurationSchemas>('/internal/v1/schemas', credential),
      ]);
      const failures = results.filter((result) => result.status === 'rejected');
      if (failures.length === results.length) throw failures[0].reason;
      const [nextTenants, nextProviders, nextPlugins, nextUpstreams, nextRequests, nextStats, nextSchemas] = results;
      if (nextTenants.status === 'fulfilled') setTenants(nextTenants.value);
      if (nextProviders.status === 'fulfilled') setProviders(nextProviders.value);
      if (nextPlugins.status === 'fulfilled') setPlugins(nextPlugins.value);
      if (nextUpstreams.status === 'fulfilled') setUpstreams(nextUpstreams.value);
      if (nextRequests.status === 'fulfilled') setRequests(nextRequests.value);
      if (nextStats.status === 'fulfilled') setStats(nextStats.value);
      if (nextSchemas.status === 'fulfilled') setSchemas(nextSchemas.value);
      if (failures.length) setError(t('common.scopeWarning', { count: failures.length }));
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : t('common.connectionFailed'));
    }
  }

  useEffect(() => { if (token) void refresh(); }, []);
  useEffect(() => { if (token) void refresh(); }, [tenant]);

  async function selectRequest(request: RequestView) {
    try {
      setError('');
      setDetail(await api<RequestDetail>(`/internal/v1/requests/${request.request_id}${queryForTenant(tenant)}`, token));
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : t('traffic.detailFailed'));
    }
  }

  useEffect(() => {
    if (!token || tab !== 'traffic') return;
    const controller = new AbortController();
    const connect = async () => {
      while (!controller.signal.aborted) {
        try {
          await streamSse<RequestEvent>(
            `/internal/v1/request-events${queryForTenant(tenant)}`,
            token,
            controller.signal,
            (event) => setRequests((current) => {
              const previous = current.find((request) => request.request_id === event.request_id);
              const next: RequestView = {
                request_id: event.request_id, created_at: previous?.created_at ?? event.event_at,
                protocol: event.protocol, model: event.model, status_code: event.status_code,
                duration_ms: event.duration_ms, input_tokens: event.input_tokens,
                output_tokens: event.output_tokens, cost: event.cost, error_code: event.error_code,
              };
              return [next, ...current.filter((request) => request.request_id !== event.request_id)]
                .sort((left, right) => right.created_at - left.created_at).slice(0, 100);
            }),
          );
        } catch (reason) {
          if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : t('traffic.streamDisconnected'));
        }
        await new Promise((resolve) => window.setTimeout(resolve, 1000));
      }
    };
    void connect();
    return () => controller.abort();
  }, [token, tab, tenant]);

  const creationTenant = tenant || tenants[0]?.external_id || 'default';
  return (
    <Shell operator>
      <header className="hero compact">
        <div><span className="eyebrow">OPERATOR CONTROL PLANE</span><h1>Token Center</h1><p>{t('operator.subtitle')}</p></div>
        <div className="credential operator-credential">
          {tenants.length > 0 && <label className="tenant-picker"><span>{t('operator.tenant')}</span><select value={tenant} onChange={(event) => setTenant(event.target.value)}><option value="">{t('operator.allTenants')}</option>{tenants.map((value) => <option key={value.external_id} value={value.external_id}>{value.external_id}</option>)}</select></label>}
          <input type="password" value={token} onChange={(event) => setToken(event.target.value)} placeholder={t('operator.tokenPlaceholder')} />
          <button onClick={() => void refresh()}>{t('common.connect')}</button>
        </div>
      </header>
      <nav className="tabs">{tabIds.map((id) => <button key={id} className={tab === id ? 'active' : ''} onClick={() => setTab(id)}>{t(`nav.${id}`)}</button>)}</nav>
      {error && <div className="notice error">{error}</div>}
      {tab === 'traffic' && <Traffic stats={stats} requests={requests} onSelect={selectRequest} />}
      {tab === 'providers' && <UpstreamProviders token={token} tenant={creationTenant} providers={providers} values={upstreams} onChanged={refresh} />}
      {tab === 'routes' && <RouteForm token={token} tenant={creationTenant} upstreams={upstreams} />}
      {tab === 'pricing' && <Pricing token={token} tenant={tenant} schemas={schemas} />}
      {tab === 'credentials' && <CredentialForm token={token} tenant={creationTenant} schema={schemas?.key_create} />}
      {tab === 'services' && <ServiceCredentialForm token={token} schema={schemas?.service_token} />}
      {tab === 'plugins' && <Plugins values={plugins} />}
      {detail && <RequestDrawer detail={detail} onClose={() => setDetail(undefined)} />}
    </Shell>
  );
}

function Traffic({ stats, requests, onSelect }: { stats?: OperatorStats; requests: RequestView[]; onSelect: (request: RequestView) => Promise<void> }) {
  const { t } = useI18n();
  return <>{stats && <>
    <section className="metrics" style={{ gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))' }}>
      <Metric label={t('traffic.total')} value={stats.summary.total_requests} />
      <Metric label={t('traffic.success')} value={stats.summary.successful_requests} tone="positive" />
      <Metric label={t('traffic.failure')} value={stats.summary.failed_requests} tone="negative" />
      <Metric label="Tokens" value={stats.summary.input_tokens + stats.summary.output_tokens} />
      <Metric label={t('traffic.cost')} value={stats.summary.total_cost} />
    </section>
    <section className="two-column"><article className="panel"><h2>{t('traffic.models')}</h2><Buckets values={stats.by_model} /></article><article className="panel"><h2>{t('traffic.days')}</h2><Buckets values={stats.by_day} /></article></section>
    {stats.errors.length > 0 && <article className="panel"><h2>{t('traffic.errors')}</h2><Buckets values={stats.errors} /></article>}
  </>}
  <article className="panel"><div className="panel-title"><h2>{t('traffic.live')}</h2><span>{t('traffic.liveHint')}</span></div><RequestTable requests={requests} onSelect={(request) => void onSelect(request)} /></article>
  </>;
}

function UpstreamProviders({ token, tenant, providers, values, onChanged }: { token: string; tenant: string; providers: ProviderType[]; values: UpstreamAccount[]; onChanged: () => Promise<void> }) {
  const { locale, t } = useI18n();
  const [method, setMethod] = useState<'direct' | 'authorization'>('direct');
  const [driver, setDriver] = useState('');
  const provider = providers.find((value) => value.id === driver) ?? providers[0];
  const schema = useMemo<RJSFSchema | undefined>(() => {
    if (!provider) return undefined;
    const config = structuredClone(provider.config_schema) as { properties?: Record<string, unknown> };
    if (provider.id === 'http-json' && config.properties) {
      delete config.properties.oauth;
      delete config.properties.timeout_seconds;
    }
    const credential = structuredClone(provider.credential_schema) as { oneOf?: Array<Record<string, unknown>> };
    if (provider.id === 'http-json' && credential.oneOf) {
      credential.oneOf = credential.oneOf
        .filter((option) => option.title !== 'OAuth')
        .sort((left) => left.title === 'API key' ? -1 : 1)
        .map((option) => {
          if (option.title !== 'API key') return option;
          const compact = structuredClone(option) as { properties?: Record<string, unknown> };
          if (compact.properties) { delete compact.properties.header; delete compact.properties.prefix; }
          return compact;
        });
    }
    return localizeSchema({
      type: 'object', required: ['name', 'config', 'credential'], properties: {
        name: { type: 'string', title: t('providers.name') },
        driver: { type: 'string', default: provider.id, readOnly: true },
        config: { ...config, title: 'Connection configuration' },
        credential: { ...credential, title: 'Access credential' },
      },
    } as RJSFSchema, locale);
  }, [provider, locale]);
  const uiSchema = { driver: { 'ui:widget': 'hidden' }, config: { oauth: { 'ui:widget': 'hidden' }, timeout_seconds: { 'ui:widget': 'hidden' } } };
  return <section className="provider-layout">
    <article className="panel provider-list"><div className="panel-title"><div><h2>{t('providers.title')}</h2><p className="muted">{t('providers.description')}</p></div><span>{values.length}</span></div>
      <div className="account-list">{values.length === 0 && <div className="empty">{t('providers.empty')}</div>}{values.map((value) => <div className="account provider-account" key={value.id}><div><b>{value.name}</b><span>{value.driver} · {t('providers.authKind')}: {value.auth_kind}{value.tenant_external_id ? ` · ${value.tenant_external_id}` : ''}</span><small>{value.id}</small></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : 'pending'}`}>{value.status}</span><span className="pill">{t('providers.generation')} {value.credential_generation}</span></div></div>)}</div>
    </article>
    <article className="panel form-panel provider-onboarding"><h2>{t('providers.add')}</h2>
      <div className="segmented"><button className={method === 'direct' ? 'active' : ''} onClick={() => setMethod('direct')}>{t('providers.direct')}</button><button className={method === 'authorization' ? 'active' : ''} onClick={() => setMethod('authorization')}>{t('providers.oauth')}</button></div>
      {method === 'direct' ? <>
        <label>{t('providers.provider')}<select value={provider?.id ?? ''} onChange={(event) => setDriver(event.target.value)}>{providers.map((value) => <option key={value.id} value={value.id}>{value.display_name} · {value.source}</option>)}</select></label>
        {schema ? <Form key={`${provider.id}-${locale}`} schema={schema} uiSchema={uiSchema} validator={validator} onSubmit={async ({ formData }) => { await api('/internal/v1/upstreams', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant }) }); await onChanged(); }}><button type="submit">{t('providers.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
      </> : <AuthorizationConnection token={token} tenant={tenant} providers={providers} onChanged={onChanged} />}
    </article>
  </section>;
}

function AuthorizationConnection({ token, tenant, providers, onChanged }: { token: string; tenant: string; providers: ProviderType[]; onChanged: () => Promise<void> }) {
  const { locale, t } = useI18n();
  const adapterProviders = providers.filter((provider) => provider.oauth_adapter);
  const [mode, setMode] = useState<'subscription' | 'cursor-direct' | 'plugin-adapter'>('subscription');
  const [subscriptionProvider, setSubscriptionProvider] = useState<'copilot' | 'cursor'>('copilot');
  const [driver, setDriver] = useState(adapterProviders[0]?.id ?? '');
  const adapter = adapterProviders.find((provider) => provider.id === driver) ?? adapterProviders[0];
  const [name, setName] = useState('copilot-primary');
  const [baseUrl, setBaseUrl] = useState('http://cpa-subscription-bridge:8080');
  const [bridgeSecret, setBridgeSecret] = useState('');
  const [session, setSession] = useState<{ login_url: string; session_token: string }>();
  const [message, setMessage] = useState('');
  const reset = () => { setSession(undefined); setMessage(''); };
  const start = async (providerConfig?: unknown) => {
    if (mode === 'subscription') {
      setSession(await api('/internal/v1/oauth/subscription-bridge/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider: subscriptionProvider, base_url: baseUrl, ...(bridgeSecret ? { bridge_secret: bridgeSecret } : {}) }) }));
    } else if (mode === 'cursor-direct') {
      setSession(await api('/internal/v1/oauth/cursor/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider_config: { base_url: baseUrl } }) }));
    } else if (adapter) {
      setSession(await api('/internal/v1/oauth/provider-adapter/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider_driver: adapter.id, provider_config: providerConfig }) }));
    }
    setMessage('');
  };
  const poll = async () => {
    if (!session) return;
    const path = mode === 'subscription' ? '/internal/v1/oauth/subscription-bridge/poll' : '/internal/v1/oauth/provider-adapter/poll';
    const actualPath = mode === 'cursor-direct' ? '/internal/v1/oauth/cursor/poll' : path;
    const result = await api<UpstreamAccount | { status: string; message?: string }>(actualPath, token, { method: 'POST', body: JSON.stringify({ session_token: session.session_token }) });
    if ('id' in result) { setMessage(t('providers.ready', { id: result.id })); await onChanged(); } else setMessage(result.message ?? t('providers.waiting'));
  };
  return <div className="authorization-form"><p className="muted">{t('providers.oauthSecurity')}</p>
    <label>{t('providers.method')}<select value={mode} onChange={(event) => { const next = event.target.value as typeof mode; setMode(next); reset(); if (next === 'subscription') { setName(`${subscriptionProvider}-primary`); setBaseUrl('http://cpa-subscription-bridge:8080'); } else if (next === 'cursor-direct') { setName('cursor-primary'); setBaseUrl('http://cursor-adapter:8080'); } else if (adapter) setName(`${adapter.id}-primary`); }}><option value="subscription">{t('providers.subscription')}</option><option value="cursor-direct">{t('providers.cursorDirect')}</option>{adapterProviders.length > 0 && <option value="plugin-adapter">{t('providers.pluginAdapter')}</option>}</select></label>
    {mode === 'subscription' && <label>{t('providers.subscriptionProvider')}<select value={subscriptionProvider} onChange={(event) => { const next = event.target.value as typeof subscriptionProvider; setSubscriptionProvider(next); setName(`${next}-primary`); reset(); }}><option value="copilot">GitHub Copilot</option><option value="cursor">Cursor</option></select></label>}
    {mode === 'plugin-adapter' && adapter && <label>{t('providers.provider')}<select value={adapter.id} onChange={(event) => { const next = event.target.value; setDriver(next); setName(`${next}-primary`); reset(); }}>{adapterProviders.map((value) => <option key={value.id} value={value.id}>{value.display_name} · {value.source}</option>)}</select></label>}
    {mode === 'plugin-adapter' && !adapter && <div className="empty">{t('providers.noAdapter')}</div>}
    <label>{t('providers.name')}<input value={name} onChange={(event) => setName(event.target.value)} /></label>
    {mode !== 'plugin-adapter' && <label>{mode === 'subscription' ? 'Bridge URL' : 'Provider Adapter URL'}<input value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} /></label>}
    {mode === 'subscription' && <label>{t('providers.bridgeSecret')}<input type="password" value={bridgeSecret} onChange={(event) => setBridgeSecret(event.target.value)} /></label>}
    {mode === 'plugin-adapter' && adapter && !session ? <Form key={`${adapter.id}-${locale}`} schema={localizeSchema(adapter.config_schema as RJSFSchema, locale)} validator={validator} onSubmit={({ formData }) => void start(formData)}><button type="submit">{t('common.startLogin')}</button></Form> : <div className="button-row"><button onClick={() => void start()} disabled={Boolean(session)}>{t('common.startLogin')}</button>{session && <><a className="button secondary" href={session.login_url} target="_blank" rel="noreferrer">{t('common.openAuthorization')}</a><button onClick={() => void poll()}>{t('common.checkAuthorization')}</button></>}</div>}
    {message && <div className="notice success">{message}</div>}
  </div>;
}

function Pricing({ token, tenant, schemas }: { token: string; tenant: string; schemas?: ConfigurationSchemas }) {
  const { locale, t } = useI18n();
  const [prices, setPrices] = useState<ModelPriceView[]>([]);
  const [usage, setUsage] = useState<ModelPriceUsageSummary>({ models: [] });
  const [syncResult, setSyncResult] = useState<ModelPriceSyncResult>();
  const [syncing, setSyncing] = useState(false);
  const [error, setError] = useState('');
  const [kind, setKind] = useState<'token' | 'generation'>('token');
  const [model, setModel] = useState('');
  const [currency, setCurrency] = useState('USD');
  const [message, setMessage] = useState('');
  const scope = queryForTenant(tenant);
  const load = async () => {
    if (!token) return;
    try {
      const [nextPrices, nextUsage] = await Promise.all([api<ModelPriceView[]>('/internal/v1/model-prices?currency=USD', token), api<ModelPriceUsageSummary>(`/internal/v1/model-prices/usage-summary${scope}`, token)]);
      setPrices(nextPrices); setUsage(nextUsage); setError('');
    } catch (reason) { setError(reason instanceof Error ? reason.message : t('common.requestFailed')); }
  };
  useEffect(() => { void load(); }, [token, tenant]);
  const usageByModel = new Map(usage.models.map((value) => [value.model, value]));
  const rows = Array.from(new Set([...usage.models.map((value) => value.model), ...prices.map((value) => value.model)])).sort().map((name) => ({ model: name, usage: usageByModel.get(name), price: prices.find((value) => value.model === name) }));
  const schema = kind === 'generation' ? schemas?.generation_price : schemas?.model_price;
  const sync = async () => {
    setSyncing(true); setError('');
    try {
      const result = await api<ModelPriceSyncResult>('/internal/v1/model-prices/sync', token, { method: 'POST', body: JSON.stringify({ models: usage.models.map((value) => value.model), currency: 'USD', ...(tenant ? { tenant_external_id: tenant } : {}) }) });
      setSyncResult(result); setPrices(result.prices);
    } catch (reason) { setError(reason instanceof Error ? reason.message : t('common.requestFailed')); }
    finally { setSyncing(false); }
  };
  return <div className="pricing-page">
    <article className="panel pricing-overview"><div className="panel-title"><div><h2>{t('pricing.title')}</h2><p className="muted">{t('pricing.description')}</p></div><button onClick={() => void sync()} disabled={syncing}>{syncing ? t('pricing.syncing') : t('pricing.sync')}</button></div>
      <div className="pricing-summary"><span>{t('pricing.usedModels', { count: usage.models.length })}</span><span>{t('pricing.saved', { count: prices.length })}</span><span>{t('pricing.sourceOrder')}: models.dev → LiteLLM → OpenRouter</span></div>
      {error && <div className="notice error">{error}</div>}
      {syncResult && <><div className="source-status">{syncResult.sourceResults.map((source) => <div className={`source-card ${source.error ? 'failed' : 'healthy'}`} key={source.source}><b>{source.source}</b><span>{source.error ? t('pricing.sourceFailed') : t('pricing.sourceHealthy', { count: source.models })}</span></div>)}</div><div className="notice success"><b>{t('pricing.result')}</b> · {t('pricing.imported', { count: syncResult.imported })} · {t('pricing.candidates', { count: syncResult.candidates.length })} · {t('pricing.unmatched', { count: syncResult.unmatched.length })} · {t('pricing.preserved', { count: syncResult.preserved.length })}</div></>}
      <div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.calls')}</th><th>{t('pricing.input')}</th><th>{t('pricing.output')}</th><th>{t('pricing.source')}</th><th>{t('pricing.updated')}</th></tr></thead><tbody>{rows.map((row) => <tr key={row.model}><td><code>{row.model}</code></td><td>{row.usage?.calls ?? 0}</td><td>{row.price ? `$${row.price.input_per_million}` : '—'}</td><td>{row.price ? `$${row.price.output_per_million}` : '—'}</td><td>{row.price ? <span className={`pill source-${row.price.source.replace('.', '-')}`}>{row.price.source}</span> : <span className="status pending">{t('pricing.missing')}</span>}</td><td>{row.price ? new Date(row.price.updated_at).toLocaleString(locale) : '—'}</td></tr>)}</tbody></table>{rows.length === 0 && <div className="empty">{t('pricing.noPrices')}</div>}</div>
    </article>
    <details className="panel manual-pricing"><summary><span><b>{t('pricing.manual')}</b><small>{t('pricing.manualHint')}</small></span><span>＋</span></summary><div className="manual-pricing-body form-panel"><label>{t('pricing.type')}<select value={kind} onChange={(event) => setKind(event.target.value as typeof kind)}><option value="token">{t('pricing.tokenModel')}</option><option value="generation">{t('pricing.generationModel')}</option></select></label><label>{t('pricing.model')}<input value={model} onChange={(event) => setModel(event.target.value)} /></label><label>{t('pricing.currency')}<input value={currency} onChange={(event) => setCurrency(event.target.value.toUpperCase())} maxLength={3} /></label>{schema ? <Form key={`${kind}-${locale}`} schema={localizeSchema(schema as RJSFSchema, locale)} validator={validator} onSubmit={async ({ formData }) => { const prefix = kind === 'generation' ? 'generation-prices' : 'prices'; await api(`/internal/v1/${prefix}/${encodeURIComponent(currency)}/${encodeURIComponent(model)}`, token, { method: 'POST', body: JSON.stringify(formData) }); setMessage(t('pricing.savedMessage')); await load(); }}><button type="submit">{t('pricing.save')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}{message && <div className="notice success">{message}</div>}</div></details>
  </div>;
}

function RouteForm({ token, tenant, upstreams }: { token: string; tenant: string; upstreams: UpstreamAccount[] }) {
  const { t } = useI18n();
  const [form, setForm] = useState({ public_model: '', upstream_account_id: '', upstream_model: '', protocol: 'openai', priority: 0 });
  const [message, setMessage] = useState('');
  return <article className="panel form-panel narrow"><h2>{t('routes.title')}</h2><label>{t('routes.publicModel')}<input value={form.public_model} onChange={(event) => setForm({ ...form, public_model: event.target.value })} /></label><label>{t('routes.upstream')}<select value={form.upstream_account_id} onChange={(event) => setForm({ ...form, upstream_account_id: event.target.value })}><option value="">{t('common.select')}</option>{upstreams.map((value) => <option key={value.id} value={value.id}>{value.name}</option>)}</select></label><label>{t('routes.upstreamModel')}<input value={form.upstream_model} onChange={(event) => setForm({ ...form, upstream_model: event.target.value })} /></label><label>{t('routes.protocol')}<select value={form.protocol} onChange={(event) => setForm({ ...form, protocol: event.target.value })}><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="generation">{t('routes.generation')}</option></select></label><button onClick={async () => { await api('/internal/v1/model-routes', token, { method: 'POST', body: JSON.stringify({ ...form, tenant_external_id: tenant }) }); setMessage(t('routes.created')); }}>{t('routes.create')}</button>{message && <div className="notice success">{message}</div>}</article>;
}

function CredentialForm({ token, tenant, schema }: { token: string; tenant: string; schema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const [result, setResult] = useState('');
  return <article className="panel form-panel narrow"><h2>{t('credentials.title')}</h2><p className="muted">{t('credentials.description')}</p>{schema ? <Form key={locale} schema={localizeSchema(schema as RJSFSchema, locale)} validator={validator} onSubmit={async ({ formData }) => { const created = await api<{ key: string }>('/internal/v1/keys', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant }) }); setResult(created.key); }}><button type="submit">{t('credentials.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}{result && <div className="one-time"><b>{t('credentials.created')}</b><code>{result}</code></div>}</article>;
}

function ServiceCredentialForm({ token, schema }: { token: string; schema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const [result, setResult] = useState('');
  return <article className="panel form-panel narrow"><h2>{t('services.title')}</h2><p className="muted">{t('services.description')}</p>{schema ? <Form key={locale} schema={localizeSchema(schema as RJSFSchema, locale)} validator={validator} onSubmit={async ({ formData }) => { const created = await api<{ token: string }>('/internal/v1/service-tokens', token, { method: 'POST', body: JSON.stringify(formData) }); setResult(created.token); }}><button type="submit">{t('services.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}{result && <div className="one-time"><b>{t('common.oneTime')}</b><code>{result}</code></div>}</article>;
}

function Plugins({ values }: { values: PluginManifest[] }) {
  const { t } = useI18n();
  return <article className="panel"><div className="panel-title"><h2>{t('plugins.title')}</h2><span>Wasmtime Component · fail-closed capabilities</span></div><div className="account-list">{values.length === 0 && <div className="empty">{t('plugins.empty')}</div>}{values.map((value) => <div className="account" key={value.id}><div><b>{value.id}</b><span>v{value.version} · WIT {value.wit_version} · {(value.contributions.providers ?? []).length} provider</span></div><span className="pill">{value.contributions.traffic_policy ? 'traffic policy' : 'provider'}</span></div>)}</div></article>;
}

function RequestDrawer({ detail, onClose }: { detail: RequestDetail; onClose: () => void }) {
  const { t } = useI18n();
  return <div className="drawer-backdrop" onClick={onClose}><aside className="drawer" onClick={(event) => event.stopPropagation()}><button className="close" onClick={onClose}>×</button><span className="eyebrow">OPERATOR REQUEST DIAGNOSIS</span><h2>{detail.model}</h2><p className="muted">{detail.request_id} · {detail.status_code ?? t('common.running')} · {detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</p><h3>{t('request.error')}</h3><pre>{detail.error_code ?? t('common.none')}</pre><h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre><h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre></aside></div>;
}
