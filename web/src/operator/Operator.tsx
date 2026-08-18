import RjsfForm, { type FormProps } from '@rjsf/core';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { api, streamSse } from '../api';
import { DrawerFrame, RequestTable, Shell } from '../components';
import { formatCurrency, formatNumber } from '../format';
import { localizeSchema, useI18n } from '../i18n';
import { LimitSnapshot } from '../LimitSnapshot';
import { schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator as validator } from '../safeValidator';
import type {
  ConfigurationSchemas, GenerationPriceView, KeyLimitSnapshot, KeyView, ModelPriceSyncResult,
  ModelPriceUsageSummary, ModelPriceView, ModelRouteView,
  PluginManifest, ProviderType, RequestDetail, RequestEvent, RequestView,
  ServiceTokenView, TenantView, UpstreamAccount, UpstreamHealth,
} from '../types';
import './operator.css';
import { UsageAnalysis } from './UsageAnalysis';

type Tab = 'traffic' | 'usage' | 'providers' | 'routes' | 'pricing' | 'credentials' | 'services' | 'plugins';
type Translate = (key: string, variables?: Record<string, string | number>) => string;
interface RequestFilters {
  from: string;
  to: string;
  keyId: string;
  model: string;
  protocol: string;
  status: string;
  errorCode: string;
  upstreamAccountId: string;
  routeId: string;
  minDurationMs: string;
  maxDurationMs: string;
  minCost: string;
  maxCost: string;
  keyAlias: string;
  principal: string;
}
interface RequestEventCursor {
  eventAt: number;
  eventId: string;
}
interface RequestEventScope {
  credential: string;
  tenant: string;
  filters: RequestFilters;
}
const tabIds: Tab[] = ['traffic', 'usage', 'providers', 'routes', 'pricing', 'credentials', 'services', 'plugins'];
const emptyRequestFilters: RequestFilters = {
  from: '', to: '', keyId: '', model: '', protocol: '', status: '', errorCode: '', upstreamAccountId: '',
  routeId: '', minDurationMs: '', maxDurationMs: '', minCost: '', maxCost: '', keyAlias: '', principal: '',
};

function Form(props: FormProps) {
  return <RjsfForm {...props} noHtml5Validate onError={() => { /* Validation is rendered inline. */ }} />;
}

function queryForTenant(tenant: string, existing = '') {
  const params = new URLSearchParams(existing);
  if (tenant) params.set('tenant_external_id', tenant);
  const query = params.toString();
  return query ? `?${query}` : '';
}

function requestQuery(tenant: string, filters: RequestFilters, before?: RequestView) {
  const params = new URLSearchParams({ limit: '100' });
  if (tenant) params.set('tenant_external_id', tenant);
  const from = filters.from ? Date.parse(filters.from) : Number.NaN;
  const to = filters.to ? Date.parse(filters.to) : Number.NaN;
  if (Number.isFinite(from)) params.set('from_created_at', String(from));
  if (Number.isFinite(to)) params.set('to_created_at', String(to));
  if (filters.keyId.trim()) params.set('key_id', filters.keyId.trim());
  if (filters.model.trim()) params.set('model', filters.model.trim());
  if (filters.protocol) params.set('protocol', filters.protocol);
  if (filters.status) params.set('status', filters.status);
  if (filters.errorCode.trim()) params.set('error_code', filters.errorCode.trim());
  if (filters.upstreamAccountId) params.set('upstream_account_id', filters.upstreamAccountId);
  if (filters.routeId.trim()) params.set('route_id', filters.routeId.trim());
  if (filters.minDurationMs.trim()) params.set('min_duration_ms', filters.minDurationMs.trim());
  if (filters.maxDurationMs.trim()) params.set('max_duration_ms', filters.maxDurationMs.trim());
  if (filters.minCost.trim()) params.set('min_cost', filters.minCost.trim());
  if (filters.maxCost.trim()) params.set('max_cost', filters.maxCost.trim());
  if (filters.keyAlias.trim()) params.set('key_alias', filters.keyAlias.trim());
  if (filters.principal.trim()) params.set('principal', filters.principal.trim());
  if (before) { params.set('before_created_at', String(before.created_at)); params.set('before_id', before.request_id); }
  return `?${params}`;
}

function requestEventQuery(tenant: string, cursor?: RequestEventCursor) {
  const params = new URLSearchParams();
  if (tenant) params.set('tenant_external_id', tenant);
  if (cursor) {
    params.set('after_event_at', String(cursor.eventAt));
    params.set('after_event_id', cursor.eventId);
  }
  const query = params.toString();
  return query ? `?${query}` : '';
}

function isAfterCursor(event: RequestEvent, cursor?: RequestEventCursor) {
  return !cursor || event.event_at > cursor.eventAt
    || (event.event_at === cursor.eventAt && event.event_id > cursor.eventId);
}

function requestViewFromEvent(event: RequestEvent, previous?: RequestView): RequestView {
  return {
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
}

function mergeLiveRequestEvents(snapshot: RequestView[], liveEvents: Map<string, RequestEvent>) {
  const merged = new Map(snapshot.map((request) => [request.request_id, request]));
  for (const event of liveEvents.values()) {
    merged.set(event.request_id, requestViewFromEvent(event, merged.get(event.request_id)));
  }
  return [...merged.values()]
    .sort((left, right) => right.created_at - left.created_at)
    .slice(0, 100);
}

function scopeMatches(scope: RequestEventScope | undefined, credential: string, tenant: string, filters: RequestFilters) {
  return scope?.credential === credential && scope.tenant === tenant && scope.filters === filters;
}

function waitForReconnect(signal: AbortSignal, milliseconds: number): Promise<void> {
  if (signal.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    const timeout = window.setTimeout(finish, milliseconds);
    signal.addEventListener('abort', finish, { once: true });
    function finish() {
      window.clearTimeout(timeout);
      signal.removeEventListener('abort', finish);
      resolve();
    }
  });
}

function filtersActive(filters: RequestFilters) {
  return Object.values(filters).some(Boolean);
}

function messageOf(reason: unknown, fallback: string) {
  return reason instanceof Error ? reason.message : fallback;
}

function enumLabel(t: Translate, prefix: string, value: string) {
  const key = `${prefix}.${value}`;
  const translated = t(key);
  return translated === key ? value : translated;
}

function WriteScopeNotice({ tenant }: { tenant: string }) {
  const { t } = useI18n();
  if (tenant) return null;
  return <div className="notice warning" role="status">{t('operator.selectTenantToWrite')}</div>;
}

function OneTimeSecret({ value, message }: { value: string; message: string }) {
  const { t } = useI18n();
  const [copied, setCopied] = useState(false);
  return <div className="one-time" role="status">
    <b>{message}</b>
    <code>{value}</code>
    <button type="button" className="secondary" onClick={async () => { await navigator.clipboard.writeText(value); setCopied(true); }}>
      {copied ? t('common.copied') : t('common.copySecret')}
    </button>
  </div>;
}

export function Operator() {
  const { locale, t } = useI18n();
  const [token, setToken] = useState('');
  const [tab, setTab] = useState<Tab>('traffic');
  const [tenant, setTenant] = useState('');
  const [tenants, setTenants] = useState<TenantView[]>([]);
  const [providers, setProviders] = useState<ProviderType[]>([]);
  const [plugins, setPlugins] = useState<PluginManifest[]>([]);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [requestFilters, setRequestFilters] = useState<RequestFilters>(emptyRequestFilters);
  const [requestsLoading, setRequestsLoading] = useState(false);
  const [hasOlderRequests, setHasOlderRequests] = useState(false);
  const [detail, setDetail] = useState<RequestDetail>();
  const [schemas, setSchemas] = useState<ConfigurationSchemas>();
  const [error, setError] = useState('');
  const [streamError, setStreamError] = useState('');
  const requestEventCursor = useRef<RequestEventCursor | undefined>(undefined);
  const requestEventScope = useRef<RequestEventScope | undefined>(undefined);
  const liveRequestEvents = useRef(new Map<string, RequestEvent>());

  async function refresh() {
    const refreshCredential = token;
    const refreshTenant = tenant;
    const refreshFilters = requestFilters;
    const credential = token.trim();
    if (!credential) return;
    setError('');
    const scope = queryForTenant(tenant);
    try {
      const results = await Promise.allSettled([
        api<TenantView[]>('/internal/v1/tenants', credential),
        api<ProviderType[]>('/internal/v1/provider-types', credential),
        api<PluginManifest[]>('/internal/v1/plugins', credential),
        api<UpstreamAccount[]>(`/internal/v1/upstreams${scope}`, credential),
        api<RequestView[]>(`/internal/v1/requests${requestQuery(tenant, requestFilters)}`, credential),
        api<ConfigurationSchemas>('/internal/v1/schemas', credential),
      ]);
      const failures = results.filter((result) => result.status === 'rejected');
      if (failures.length === results.length) throw failures[0].reason;
      const [nextTenants, nextProviders, nextPlugins, nextUpstreams, nextRequests, nextSchemas] = results;
      setTenants(nextTenants.status === 'fulfilled' ? nextTenants.value : []);
      setProviders(nextProviders.status === 'fulfilled' ? nextProviders.value : []);
      setPlugins(nextPlugins.status === 'fulfilled' ? nextPlugins.value : []);
      setUpstreams(nextUpstreams.status === 'fulfilled' ? nextUpstreams.value : []);
      if (scopeMatches(requestEventScope.current, refreshCredential, refreshTenant, refreshFilters)) {
        setRequests(nextRequests.status === 'fulfilled'
          ? mergeLiveRequestEvents(nextRequests.value, liveRequestEvents.current)
          : []);
        setHasOlderRequests(nextRequests.status === 'fulfilled' && nextRequests.value.length === 100);
      }
      setSchemas(nextSchemas.status === 'fulfilled' ? nextSchemas.value : undefined);
      if (failures.length) setError(t('common.scopeWarning', { count: formatNumber(failures.length, locale) }));
    } catch (reason) {
      if (!scopeMatches(requestEventScope.current, refreshCredential, refreshTenant, refreshFilters)) return;
      setTenants([]); setProviders([]); setPlugins([]); setUpstreams([]); setRequests([]);
      setSchemas(undefined); setHasOlderRequests(false);
      setError(messageOf(reason, t('common.connectionFailed')));
    }
  }

  useEffect(() => { if (token) void refresh(); }, []);
  useEffect(() => { if (token) void refresh(); }, [tenant]);

  async function loadRequests(filters: RequestFilters, older = false) {
    if (!token) return;
    const loadCredential = token;
    const loadTenant = tenant;
    const before = older ? requests.at(-1) : undefined;
    setRequestsLoading(true); setError('');
    try {
      const next = await api<RequestView[]>(`/internal/v1/requests${requestQuery(tenant, filters, before)}`, token);
      if (scopeMatches(requestEventScope.current, loadCredential, loadTenant, filters)) {
        setRequests((current) => older
          ? [...current, ...next.filter((value) => !current.some((existing) => existing.request_id === value.request_id))]
          : mergeLiveRequestEvents(next, liveRequestEvents.current));
        setHasOlderRequests(next.length === 100);
      }
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setRequestsLoading(false); }
  }

  async function selectRequest(request: RequestView) {
    try {
      setError('');
      setDetail(await api<RequestDetail>(`/internal/v1/requests/${request.request_id}${queryForTenant(tenant)}`, token));
    } catch (reason) {
      setError(messageOf(reason, t('traffic.detailFailed')));
    }
  }

  useEffect(() => {
    const previousScope = requestEventScope.current;
    if (!previousScope || previousScope.credential !== token || previousScope.tenant !== tenant
      || previousScope.filters !== requestFilters) {
      requestEventCursor.current = undefined;
      liveRequestEvents.current.clear();
      requestEventScope.current = { credential: token, tenant, filters: requestFilters };
      setStreamError('');
    }
    if (!token || tab !== 'traffic' || filtersActive(requestFilters)) {
      setStreamError('');
      return;
    }
    const activeScope = requestEventScope.current;
    const controller = new AbortController();
    const connect = async () => {
      while (!controller.signal.aborted) {
        try {
          await streamSse<RequestEvent>(
            `/internal/v1/request-events${requestEventQuery(tenant, requestEventCursor.current)}`,
            token,
            controller.signal,
            ({ id, event: eventName, data: event }) => {
              if (controller.signal.aborted || requestEventScope.current !== activeScope) return;
              if (id !== event.event_id) throw new Error('SSE id does not match request event_id');
              if (eventName !== `request.${event.event_kind}`) throw new Error('SSE event name does not match request event_kind');
              if (!isAfterCursor(event, requestEventCursor.current)) return;
              requestEventCursor.current = { eventAt: event.event_at, eventId: id };
              liveRequestEvents.current.delete(event.request_id);
              liveRequestEvents.current.set(event.request_id, event);
              while (liveRequestEvents.current.size > 200) {
                const oldestRequestId = liveRequestEvents.current.keys().next().value as string | undefined;
                if (!oldestRequestId) break;
                liveRequestEvents.current.delete(oldestRequestId);
              }
              setStreamError('');
              setRequests((current) => {
                const previous = current.find((request) => request.request_id === event.request_id);
                const next = requestViewFromEvent(event, previous);
                return [next, ...current.filter((request) => request.request_id !== event.request_id)]
                  .sort((left, right) => right.created_at - left.created_at).slice(0, 100);
              });
            },
          );
        } catch (reason) {
          if (!controller.signal.aborted) setStreamError(messageOf(reason, t('traffic.streamDisconnected')));
        }
        await waitForReconnect(controller.signal, 1000);
      }
    };
    void connect();
    return () => controller.abort();
  }, [token, tab, tenant, requestFilters]);

  const changeTabByKeyboard = (event: KeyboardEvent<HTMLButtonElement>, current: Tab) => {
    const currentIndex = tabIds.indexOf(current);
    let nextIndex = currentIndex;
    if (event.key === 'ArrowRight') nextIndex = (currentIndex + 1) % tabIds.length;
    else if (event.key === 'ArrowLeft') nextIndex = (currentIndex - 1 + tabIds.length) % tabIds.length;
    else if (event.key === 'Home') nextIndex = 0;
    else if (event.key === 'End') nextIndex = tabIds.length - 1;
    else return;
    event.preventDefault();
    setTab(tabIds[nextIndex]);
    requestAnimationFrame(() => document.getElementById(`operator-tab-${tabIds[nextIndex]}`)?.focus());
  };

  return <Shell operator>
    <header className="hero compact">
      <div><span className="eyebrow">{t('operator.eyebrow')}</span><h1>Token Center</h1><p>{t('operator.subtitle')}</p></div>
      <div className="credential operator-credential">
        {tenants.length > 0 && <label className="tenant-picker"><span>{t('operator.tenant')}</span><select value={tenant} onChange={(event) => setTenant(event.target.value)}><option value="">{t('operator.allTenants')}</option>{tenants.map((value) => <option key={value.external_id} value={value.external_id}>{value.external_id}</option>)}</select></label>}
        <input aria-label={t('operator.serviceCredential')} autoComplete="off" type="password" value={token} onChange={(event) => setToken(event.target.value)} placeholder={t('operator.tokenPlaceholder')} />
        <button type="button" onClick={() => void refresh()}>{t('common.connect')}</button>
      </div>
    </header>
    <nav className="tabs" role="tablist" aria-label={t('operator.sections')}>{tabIds.map((id) => <button id={`operator-tab-${id}`} role="tab" aria-selected={tab === id} aria-controls={`operator-panel-${id}`} tabIndex={tab === id ? 0 : -1} key={id} className={tab === id ? 'active' : ''} onClick={() => setTab(id)} onKeyDown={(event) => changeTabByKeyboard(event, id)}>{t(`nav.${id}`)}</button>)}</nav>
    {error && <div className="notice error" role="alert">{error}</div>}
    {streamError && <div className="notice error" role="alert">{streamError}</div>}
    <section id={`operator-panel-${tab}`} role="tabpanel" aria-labelledby={`operator-tab-${tab}`} tabIndex={0}>
      {tab === 'traffic' && <Traffic requests={requests} upstreams={upstreams} filters={requestFilters} loading={requestsLoading} hasOlder={hasOlderRequests} onApply={(filters) => { setRequestFilters(filters); void loadRequests(filters); }} onClear={() => { setRequestFilters(emptyRequestFilters); void loadRequests(emptyRequestFilters); }} onLoadOlder={() => void loadRequests(requestFilters, true)} onSelect={selectRequest} />}
      {tab === 'usage' && <UsageAnalysis token={token} tenant={tenant} upstreams={upstreams} />}
      {tab === 'providers' && <UpstreamProviders token={token} tenant={tenant} providers={providers} values={upstreams} onChanged={refresh} />}
      {tab === 'routes' && <RouteWorkspace token={token} tenant={tenant} upstreams={upstreams} providers={providers} />}
      {tab === 'pricing' && <Pricing token={token} tenant={tenant} schemas={schemas} />}
      {tab === 'credentials' && <CredentialWorkspace token={token} tenant={tenant} createSchema={schemas?.key_create} policySchema={schemas?.key_policy} />}
      {tab === 'services' && <ServiceCredentialWorkspace token={token} tenant={tenant} schema={schemas?.service_token} />}
      {tab === 'plugins' && <Plugins values={plugins} />}
    </section>
    {detail && <RequestDrawer detail={detail} onClose={() => setDetail(undefined)} />}
  </Shell>;
}

function Traffic({ requests, upstreams, filters, loading, hasOlder, onApply, onClear, onLoadOlder, onSelect }: { requests: RequestView[]; upstreams: UpstreamAccount[]; filters: RequestFilters; loading: boolean; hasOlder: boolean; onApply: (filters: RequestFilters) => void; onClear: () => void; onLoadOlder: () => void; onSelect: (request: RequestView) => Promise<void> }) {
  const { t } = useI18n();
  const [draft, setDraft] = useState(filters);
  useEffect(() => setDraft(filters), [filters]);
  return <article className="panel"><div className="panel-title"><div><h2>{filtersActive(filters) ? t('traffic.filtered') : t('traffic.live')}</h2><span>{filtersActive(filters) ? t('traffic.filteredHint') : t('traffic.liveHint')}</span></div></div>
    <form className="traffic-filters" onSubmit={(event) => { event.preventDefault(); onApply(draft); }}>
      <label>{t('traffic.from')}<input type="datetime-local" value={draft.from} onChange={(event) => setDraft({ ...draft, from: event.target.value })} /></label>
      <label>{t('traffic.to')}<input type="datetime-local" value={draft.to} onChange={(event) => setDraft({ ...draft, to: event.target.value })} /></label>
      <label>{t('traffic.keyId')}<input value={draft.keyId} onChange={(event) => setDraft({ ...draft, keyId: event.target.value })} placeholder="019f…" /></label>
      <label>{t('request.model')}<input value={draft.model} onChange={(event) => setDraft({ ...draft, model: event.target.value })} /></label>
      <label>{t('request.protocol')}<select value={draft.protocol} onChange={(event) => setDraft({ ...draft, protocol: event.target.value })}><option value="">{t('common.all')}</option><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="openai-image">OpenAI Image</option><option value="generation">{t('routes.generation')}</option></select></label>
      <label>{t('request.status')}<select value={draft.status} onChange={(event) => setDraft({ ...draft, status: event.target.value })}><option value="">{t('common.all')}</option><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option><option value="pending">{t('common.running')}</option></select></label>
      <label>{t('traffic.errorCode')}<input value={draft.errorCode} onChange={(event) => setDraft({ ...draft, errorCode: event.target.value })} /></label>
      <label>{t('traffic.upstream')}<select value={draft.upstreamAccountId} onChange={(event) => setDraft({ ...draft, upstreamAccountId: event.target.value })}><option value="">{t('common.all')}</option>{upstreams.map((value) => <option value={value.id} key={value.id}>{value.name}</option>)}</select></label>
      <label>{t('traffic.routeId')}<input value={draft.routeId} onChange={(event) => setDraft({ ...draft, routeId: event.target.value })} placeholder="019f…" /></label>
      <label>{t('traffic.keyAlias')}<input value={draft.keyAlias} onChange={(event) => setDraft({ ...draft, keyAlias: event.target.value })} /></label>
      <label>{t('traffic.principal')}<input value={draft.principal} onChange={(event) => setDraft({ ...draft, principal: event.target.value })} /></label>
      <label>{t('traffic.minDuration')}<input type="number" min="0" value={draft.minDurationMs} onChange={(event) => setDraft({ ...draft, minDurationMs: event.target.value })} /></label>
      <label>{t('traffic.maxDuration')}<input type="number" min="0" value={draft.maxDurationMs} onChange={(event) => setDraft({ ...draft, maxDurationMs: event.target.value })} /></label>
      <label>{t('traffic.minCost')}<input inputMode="decimal" value={draft.minCost} onChange={(event) => setDraft({ ...draft, minCost: event.target.value })} /></label>
      <label>{t('traffic.maxCost')}<input inputMode="decimal" value={draft.maxCost} onChange={(event) => setDraft({ ...draft, maxCost: event.target.value })} /></label>
      <div className="filter-actions"><button type="submit" disabled={loading}>{loading ? t('common.loading') : t('traffic.applyFilters')}</button><button type="button" className="secondary" disabled={loading || (!filtersActive(filters) && !filtersActive(draft))} onClick={() => { setDraft(emptyRequestFilters); onClear(); }}>{t('traffic.clearFilters')}</button></div>
    </form>
    <RequestTable requests={requests} onSelect={(request) => void onSelect(request)} />
    {hasOlder && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={onLoadOlder}>{loading ? t('common.loading') : t('traffic.loadOlder')}</button></div>}
  </article>;
}

function UpstreamProviders({ token, tenant, providers, values, onChanged }: { token: string; tenant: string; providers: ProviderType[]; values: UpstreamAccount[]; onChanged: () => Promise<void> }) {
  const { locale, t } = useI18n();
  const [method, setMethod] = useState<'direct' | 'authorization'>('direct');
  const [driver, setDriver] = useState('');
  const [rotating, setRotating] = useState<UpstreamAccount>();
  const [editing, setEditing] = useState<UpstreamAccount>();
  const [busy, setBusy] = useState('');
  const [health, setHealth] = useState<Record<string, UpstreamHealth>>({});
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
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
      credential.oneOf = credential.oneOf.filter((option) => option.title !== 'OAuth').sort((left) => left.title === 'API key' ? -1 : 1).map((option) => {
        if (option.title !== 'API key') return option;
        const compact = structuredClone(option) as { properties?: Record<string, unknown> };
        if (compact.properties) { delete compact.properties.header; delete compact.properties.prefix; }
        return compact;
      });
    }
    return localizeSchema({ type: 'object', required: ['name', 'config', 'credential'], properties: {
      name: { type: 'string', title: t('providers.name') },
      driver: { type: 'string', default: provider.id, readOnly: true },
      config: { ...config, title: 'Connection configuration' },
      credential: { ...credential, title: 'Access credential' },
    } } as RJSFSchema, locale);
  }, [provider, locale]);
  const rotateProvider = rotating ? providers.find((value) => value.id === rotating.driver) : undefined;
  const editProvider = editing ? providers.find((value) => value.id === editing.driver) : undefined;
  const editSchema = useMemo<RJSFSchema | undefined>(() => editing && editProvider ? localizeSchema({
    type: 'object',
    additionalProperties: false,
    required: ['name', 'config'],
    properties: {
      name: { type: 'string', minLength: 1, maxLength: 200, title: t('providers.name') },
      config: { ...structuredClone(editProvider.config_schema), title: 'Connection configuration' },
    },
  } as RJSFSchema, locale) : undefined, [editing, editProvider, locale]);
  const uiSchema = { driver: { 'ui:widget': 'hidden' }, config: { oauth: { 'ui:widget': 'hidden' }, timeout_seconds: { 'ui:widget': 'hidden' } } };
  useEffect(() => { setRotating(undefined); setEditing(undefined); setHealth({}); }, [tenant]);

  const canManage = (value: UpstreamAccount) => Boolean(tenant) && (!value.tenant_external_id || value.tenant_external_id === tenant);

  async function refreshOAuth(value: UpstreamAccount) {
    if (!canManage(value)) return;
    setBusy(`refresh-${value.id}`);
    setError(''); setMessage('');
    try {
      await api(`/internal/v1/upstreams/${value.id}/oauth/refresh`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } });
      setMessage(t('providers.refreshed', { name: value.name }));
      await onChanged();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  async function setStatus(value: UpstreamAccount, status: 'active' | 'disabled') {
    if (!canManage(value)) return;
    setBusy(`status-${value.id}`); setError(''); setMessage('');
    try {
      await api(`/internal/v1/upstreams/${value.id}`, token, { method: 'PATCH', body: JSON.stringify({ tenant_external_id: tenant, status, expected_updated_at: value.updated_at }) });
      setMessage(t(status === 'active' ? 'providers.enabled' : 'providers.disabled', { name: value.name }));
      await onChanged();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  async function checkHealth(value: UpstreamAccount) {
    if (!canManage(value)) return;
    setBusy(`health-${value.id}`); setError('');
    try {
      const result = await api<UpstreamHealth>(`/internal/v1/upstreams/${value.id}/health${queryForTenant(tenant)}`, token, { method: 'POST' });
      setHealth((current) => ({ ...current, [value.id]: result }));
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  async function remove(value: UpstreamAccount) {
    if (!canManage(value) || value.status !== 'disabled' || !window.confirm(t('providers.confirmDelete', { name: value.name }))) return;
    setBusy(`delete-${value.id}`); setError(''); setMessage('');
    try {
      const query = new URLSearchParams({ tenant_external_id: tenant, expected_updated_at: String(value.updated_at) });
      await api(`/internal/v1/upstreams/${value.id}?${query}`, token, { method: 'DELETE' });
      setMessage(t('providers.deleted', { name: value.name }));
      await onChanged();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  return <><WriteScopeNotice tenant={tenant} /><section className="provider-layout">
    <article className="panel provider-list"><div className="panel-title"><div><h2>{t('providers.title')}</h2><p className="muted">{t('providers.description')}</p></div><span>{formatNumber(values.length, locale)}</span></div>
      {error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      <div className="account-list">{values.length === 0 && <div className="empty">{t('providers.empty')}</div>}{values.map((value) => {
        const currentHealth = health[value.id];
        const manageable = canManage(value);
        return <div className="account provider-account" key={value.id}><div className="account-main"><b>{value.name}</b><span>{value.driver} · {t('providers.method')}: {enumLabel(t, 'auth', value.connection_method)}{value.tenant_external_id ? ` · ${value.tenant_external_id}` : ''}</span><small>{value.id}</small>{value.credential_expires_at && <small>{t('providers.expires')}: {new Date(value.credential_expires_at).toLocaleString(locale)}</small>}{currentHealth && <small className={`status ${currentHealth.status === 'healthy' ? 'ok' : 'pending'}`}>{currentHealth.status === 'healthy' ? t('providers.healthy') : t('providers.unhealthy')}{currentHealth.upstream_status ? ` · HTTP ${formatNumber(currentHealth.upstream_status, locale)}` : ''}{currentHealth.latency_ms !== undefined ? ` · ${formatNumber(currentHealth.latency_ms, locale, 2)} ms` : ''}</small>}</div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : 'pending'}`}>{enumLabel(t, 'status', value.status)}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span><span className="pill">{t('providers.routes', { count: formatNumber(value.route_count, locale) })}</span><div className="row-actions"><button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setEditing(value)}>{t('providers.edit')}</button><button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void checkHealth(value)}>{t('providers.health')}</button>{value.can_refresh && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void refreshOAuth(value)}>{t('providers.refreshAuthorization')}</button>}{value.can_rotate && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setRotating(value)}>{t('providers.rotateCredential')}</button>}<button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void setStatus(value, value.status === 'active' ? 'disabled' : 'active')}>{value.status === 'active' ? t('providers.disable') : t('providers.enable')}</button><button type="button" className="danger" title={value.status !== 'disabled' ? t('providers.disableBeforeDelete') : value.route_count > 0 ? t('providers.removeRoutesFirst') : undefined} disabled={!manageable || Boolean(busy) || value.status !== 'disabled' || value.route_count > 0} onClick={() => void remove(value)}>{t('common.remove')}</button></div></div></div>;
      })}</div>
      {editing && editSchema && <div className="inline-editor"><div className="panel-title"><h3>{t('providers.editFor', { name: editing.name })}</h3><button type="button" className="secondary" onClick={() => setEditing(undefined)}>{t('common.cancel')}</button></div><Form key={`${editing.id}-${locale}`} schema={editSchema} uiSchema={{ config: { oauth: { 'ui:disabled': true } } }} formData={{ name: editing.name, config: editing.config }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!formData) return; setBusy(`edit-${editing.id}`); try { await api(`/internal/v1/upstreams/${editing.id}`, token, { method: 'PUT', body: JSON.stringify({ ...formData, tenant_external_id: tenant, expected_updated_at: editing.updated_at }) }); setEditing(undefined); setMessage(t('providers.updated', { name: editing.name })); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}><button type="submit" disabled={!canManage(editing) || Boolean(busy)}>{t('common.save')}</button></Form></div>}
      {rotating && rotateProvider && <div className="inline-editor"><div className="panel-title"><h3>{t('providers.rotateFor', { name: rotating.name })}</h3><button type="button" className="secondary" onClick={() => setRotating(undefined)}>{t('common.cancel')}</button></div><Form key={`${rotating.id}-${locale}`} schema={localizeSchema(rotateProvider.credential_schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { setBusy(`rotate-${rotating.id}`); try { await api(`/internal/v1/upstreams/${rotating.id}/credential`, token, { method: 'PUT', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify({ credential: formData }) }); setRotating(undefined); setMessage(t('providers.rotated', { name: rotating.name })); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}><button type="submit" disabled={!canManage(rotating) || Boolean(busy)}>{t('providers.confirmRotate')}</button></Form></div>}
    </article>
    <article className="panel form-panel provider-onboarding"><h2>{t('providers.add')}</h2>
      <div className="segmented" role="group" aria-label={t('providers.method')}><button type="button" aria-pressed={method === 'direct'} className={method === 'direct' ? 'active' : ''} onClick={() => setMethod('direct')}>{t('providers.direct')}</button><button type="button" aria-pressed={method === 'authorization'} className={method === 'authorization' ? 'active' : ''} onClick={() => setMethod('authorization')}>{t('providers.oauth')}</button></div>
      {method === 'direct' ? <>
        <label>{t('providers.provider')}<select value={provider?.id ?? ''} onChange={(event) => setDriver(event.target.value)}>{providers.map((value) => <option key={value.id} value={value.id}>{value.display_name} · {value.source}</option>)}</select></label>
        {schema ? <Form key={`${provider.id}-${locale}`} schema={schema} uiSchema={uiSchema} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { setError(''); await api('/internal/v1/upstreams', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant }) }); setMessage(t('providers.created')); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant || !token}>{t('providers.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
      </> : <AuthorizationConnection token={token} tenant={tenant} providers={providers} onChanged={onChanged} />}
    </article>
  </section></>;
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
  const [error, setError] = useState('');
  const reset = () => { setSession(undefined); setMessage(''); setError(''); };
  useEffect(() => { setSession(undefined); setMessage(''); setError(''); }, [tenant]);
  const start = async (providerConfig?: unknown) => {
    if (!tenant) return;
    try {
      if (mode === 'subscription') setSession(await api('/internal/v1/oauth/subscription-bridge/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider: subscriptionProvider, base_url: baseUrl, ...(bridgeSecret ? { bridge_secret: bridgeSecret } : {}) }) }));
      else if (mode === 'cursor-direct') setSession(await api('/internal/v1/oauth/cursor/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider_config: { base_url: baseUrl } }) }));
      else if (adapter) setSession(await api('/internal/v1/oauth/provider-adapter/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider_driver: adapter.id, provider_config: providerConfig }) }));
      setMessage(''); setError('');
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  const poll = async () => {
    if (!session) return;
    const path = mode === 'subscription' ? '/internal/v1/oauth/subscription-bridge/poll' : mode === 'cursor-direct' ? '/internal/v1/oauth/cursor/poll' : '/internal/v1/oauth/provider-adapter/poll';
    try {
      const result = await api<UpstreamAccount | { status: string; message?: string }>(path, token, { method: 'POST', body: JSON.stringify({ session_token: session.session_token }) });
      if ('id' in result) { setMessage(t('providers.ready', { id: result.id })); setSession(undefined); await onChanged(); }
      else setMessage(result.message ?? t('providers.waiting'));
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  return <div className="authorization-form"><p className="muted">{t('providers.oauthSecurity')}</p>
    {error && <div className="notice error" role="alert">{error}</div>}
    <label>{t('providers.method')}<select value={mode} onChange={(event) => { const next = event.target.value as typeof mode; setMode(next); reset(); if (next === 'subscription') { setName(`${subscriptionProvider}-primary`); setBaseUrl('http://cpa-subscription-bridge:8080'); } else if (next === 'cursor-direct') { setName('cursor-primary'); setBaseUrl('http://cursor-adapter:8080'); } else if (adapter) setName(`${adapter.id}-primary`); }}><option value="subscription">{t('providers.subscription')}</option><option value="cursor-direct">{t('providers.cursorDirect')}</option>{adapterProviders.length > 0 && <option value="plugin-adapter">{t('providers.pluginAdapter')}</option>}</select></label>
    {mode === 'subscription' && <label>{t('providers.subscriptionProvider')}<select value={subscriptionProvider} onChange={(event) => { const next = event.target.value as typeof subscriptionProvider; setSubscriptionProvider(next); setName(`${next}-primary`); reset(); }}><option value="copilot">GitHub Copilot</option><option value="cursor">Cursor</option></select></label>}
    {mode === 'plugin-adapter' && adapter && <label>{t('providers.provider')}<select value={adapter.id} onChange={(event) => { const next = event.target.value; setDriver(next); setName(`${next}-primary`); reset(); }}>{adapterProviders.map((value) => <option key={value.id} value={value.id}>{value.display_name} · {value.source}</option>)}</select></label>}
    {mode === 'plugin-adapter' && !adapter && <div className="empty">{t('providers.noAdapter')}</div>}
    <label>{t('providers.name')}<input value={name} onChange={(event) => setName(event.target.value)} /></label>
    {mode !== 'plugin-adapter' && <label>{mode === 'subscription' ? t('providers.bridgeUrl') : t('providers.adapterUrl')}<input value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} /></label>}
    {mode === 'subscription' && <label>{t('providers.bridgeSecret')}<input type="password" value={bridgeSecret} onChange={(event) => setBridgeSecret(event.target.value)} /></label>}
    {mode === 'plugin-adapter' && adapter && !session ? <Form key={`${adapter.id}-${locale}`} schema={localizeSchema(adapter.config_schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={({ formData }) => void start(formData)}><button type="submit" disabled={!tenant}>{t('common.startLogin')}</button></Form> : <div className="button-row"><button type="button" onClick={() => void start()} disabled={!tenant || Boolean(session)}>{t('common.startLogin')}</button>{session && <><a className="button secondary" href={session.login_url} target="_blank" rel="noreferrer">{t('common.openAuthorization')}</a><button type="button" onClick={() => void poll()}>{t('common.checkAuthorization')}</button></>}</div>}
    {message && <div className="notice success" role="status">{message}</div>}
  </div>;
}

function Pricing({ token, tenant, schemas }: { token: string; tenant: string; schemas?: ConfigurationSchemas }) {
  const { locale, t } = useI18n();
  const [prices, setPrices] = useState<ModelPriceView[]>([]);
  const [generationPrices, setGenerationPrices] = useState<GenerationPriceView[]>([]);
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
    const results = await Promise.allSettled([
      api<ModelPriceView[]>('/internal/v1/model-prices?currency=USD', token),
      api<ModelPriceUsageSummary>(`/internal/v1/model-prices/usage-summary${scope}`, token),
      api<GenerationPriceView[]>('/internal/v1/generation-prices?currency=USD', token),
    ]);
    const [nextPrices, nextUsage, nextGenerationPrices] = results;
    if (nextPrices.status === 'fulfilled') setPrices(nextPrices.value);
    if (nextUsage.status === 'fulfilled') setUsage(nextUsage.value);
    if (nextGenerationPrices.status === 'fulfilled') setGenerationPrices(nextGenerationPrices.value);
    const failures = results.filter((result) => result.status === 'rejected');
    setError(failures.length ? t('pricing.partialLoad', { count: formatNumber(failures.length, locale) }) : '');
  };
  useEffect(() => { void load(); }, [token, tenant]);
  const usageByModel = new Map(usage.models.map((value) => [value.model, value]));
  const rows = Array.from(new Set([...usage.models.map((value) => value.model), ...prices.map((value) => value.model)])).sort().flatMap((name) => {
    const price = prices.find((value) => value.model === name);
    const tiers = price?.tiers?.length ? price.tiers : price ? [{ service_tier: 'default', input_per_million: price.input_per_million, cached_input_per_million: price.input_per_million, cache_write_per_million: price.input_per_million, output_per_million: price.output_per_million, source: price.source, updated_at: price.updated_at, cache_price_estimated: true }] : [undefined];
    return tiers.map((tier, index) => ({ model: name, usage: index === 0 ? usageByModel.get(name) : undefined, tier }));
  });
  const schema = kind === 'generation' ? schemas?.generation_price : schemas?.model_price;
  const sync = async () => {
    if (!tenant) return;
    setSyncing(true); setError(''); setMessage('');
    try {
      const result = await api<ModelPriceSyncResult>('/internal/v1/model-prices/sync', token, { method: 'POST', body: JSON.stringify({ models: usage.models.map((value) => value.model), currency: 'USD', tenant_external_id: tenant }) });
      setSyncResult(result); setPrices(result.prices); setMessage(t('pricing.synced', { count: formatNumber(result.imported, locale) }));
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setSyncing(false); }
  };
  return <div className="pricing-page"><WriteScopeNotice tenant={tenant} />
    <article className="panel pricing-overview"><div className="panel-title"><div><h2>{t('pricing.title')}</h2><p className="muted">{t('pricing.description')}</p></div><button type="button" onClick={() => void sync()} disabled={!tenant || syncing}>{syncing ? t('pricing.syncing') : t('pricing.sync')}</button></div>
      <div className="pricing-summary"><span>{t('pricing.usedModels', { count: formatNumber(usage.models.length, locale) })}</span><span>{t('pricing.saved', { count: formatNumber(prices.length, locale) })}</span><span>{t('pricing.sourceOrder')}: models.dev → LiteLLM → OpenRouter</span></div>
      {error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      {syncResult && <><div className="source-status">{syncResult.sourceResults.map((source) => <div className={`source-card ${source.error ? 'failed' : 'healthy'}`} key={source.source}><b>{source.source}</b><span>{source.error ? t('pricing.sourceFailed') : t('pricing.sourceHealthy', { count: formatNumber(source.models, locale) })}</span>{source.error && <small>{source.error}</small>}</div>)}</div><div className="notice success"><b>{t('pricing.result')}</b> · {t('pricing.imported', { count: formatNumber(syncResult.imported, locale) })} · {t('pricing.candidates', { count: formatNumber(syncResult.candidates.length, locale) })} · {t('pricing.unmatched', { count: formatNumber(syncResult.unmatched.length, locale) })} · {t('pricing.preserved', { count: formatNumber(syncResult.preserved.length, locale) })}</div>
        {(syncResult.candidates.length > 0 || syncResult.unmatched.length > 0) && <div className="sync-details"><h3>{t('pricing.candidateDetails')}</h3>{syncResult.candidates.map((candidate) => <details key={candidate.model}><summary><code>{candidate.model}</code><span>{t('pricing.candidateCount', { count: formatNumber(candidate.candidates.length, locale) })}</span></summary><div className="candidate-list">{candidate.candidates.map((match) => <div key={`${match.source}-${match.sourceModelId}-${match.serviceTier}`}><b>{match.sourceModelId}</b><span>{match.source} · {match.serviceTier} · {match.reason}</span><code>{t('pricing.input')}: {formatCurrency(match.inputPerMillion, 'USD', locale)} · {t('pricing.output')}: {formatCurrency(match.outputPerMillion, 'USD', locale)}</code></div>)}</div></details>)}{syncResult.unmatched.length > 0 && <details><summary>{t('pricing.unmatchedModels')}</summary><div className="tag-list">{syncResult.unmatched.map((name) => <code key={name}>{name}</code>)}</div></details>}</div>}
      </>}
      <div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.calls')}</th><th>{t('pricing.serviceTier')}</th><th>{t('pricing.input')}</th><th>{t('pricing.cachedInput')}</th><th>{t('pricing.cacheWrite')}</th><th>{t('pricing.output')}</th><th>{t('pricing.source')}</th><th>{t('pricing.updated')}</th></tr></thead><tbody>{rows.map((row) => <tr key={`${row.model}-${row.tier?.service_tier ?? 'missing'}`}><td><code>{row.model}</code></td><td>{row.usage ? formatNumber(row.usage.calls, locale) : ''}</td><td>{row.tier?.service_tier ?? '—'}</td><td>{row.tier ? formatCurrency(row.tier.input_per_million, 'USD', locale) : '—'}</td><td>{row.tier ? <>{formatCurrency(row.tier.cached_input_per_million, 'USD', locale)}{row.tier.cache_price_estimated && <small className="muted"> {t('pricing.estimated')}</small>}</> : '—'}</td><td>{row.tier ? <>{formatCurrency(row.tier.cache_write_per_million, 'USD', locale)}{row.tier.cache_price_estimated && <small className="muted"> {t('pricing.estimated')}</small>}</> : '—'}</td><td>{row.tier ? formatCurrency(row.tier.output_per_million, 'USD', locale) : '—'}</td><td>{row.tier ? <span className={`pill source-${row.tier.source.replace('.', '-')}`}>{row.tier.source}</span> : <span className="status pending">{t('pricing.missing')}</span>}</td><td>{row.tier ? new Date(row.tier.updated_at).toLocaleString(locale) : '—'}</td></tr>)}</tbody></table>{rows.length === 0 && <div className="empty">{t('pricing.noPrices')}</div>}</div>
    </article>
    <article className="panel"><div className="panel-title"><h2>{t('pricing.generationPrices')}</h2><span>{formatNumber(generationPrices.length, locale)}</span></div><div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.currency')}</th><th>{t('self.units')}</th><th>{t('pricing.unitPrice')}</th></tr></thead><tbody>{generationPrices.map((price) => <tr key={`${price.currency}-${price.model}`}><td><code>{price.model}</code></td><td>{price.currency}</td><td>{enumLabel(t, 'billingUnit', price.billing_unit)}</td><td>{formatCurrency(price.price_per_unit, price.currency, locale)}</td></tr>)}</tbody></table>{generationPrices.length === 0 && <div className="empty">{t('pricing.noGenerationPrices')}</div>}</div></article>
    <details className="panel manual-pricing"><summary><span><b>{t('pricing.manual')}</b><small>{t('pricing.manualHint')}</small></span><span>＋</span></summary><div className="manual-pricing-body form-panel"><label>{t('pricing.type')}<select value={kind} onChange={(event) => setKind(event.target.value as typeof kind)}><option value="token">{t('pricing.tokenModel')}</option><option value="generation">{t('pricing.generationModel')}</option></select></label><label>{t('pricing.model')}<input value={model} onChange={(event) => setModel(event.target.value)} /></label><label>{t('pricing.currency')}<input value={currency} onChange={(event) => setCurrency(event.target.value.toUpperCase())} maxLength={3} /></label>{schema ? <Form key={`${kind}-${locale}`} schema={localizeSchema(schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { const prefix = kind === 'generation' ? 'generation-prices' : 'prices'; await api(`/internal/v1/${prefix}/${encodeURIComponent(currency)}/${encodeURIComponent(model)}`, token, { method: 'POST', body: JSON.stringify(formData) }); setMessage(t('pricing.savedMessage')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant || !model.trim()}>{t('pricing.save')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}</div></details>
  </div>;
}

type RouteDraft = Pick<ModelRouteView, 'public_model' | 'upstream_account_id' | 'upstream_model' | 'protocol' | 'priority'>;
const emptyRouteDraft: RouteDraft = { public_model: '', upstream_account_id: '', upstream_model: '', protocol: 'openai', priority: 0 };

function RouteFields({ draft, upstreams, providers, onChange }: { draft: RouteDraft; upstreams: UpstreamAccount[]; providers: ProviderType[]; onChange: (draft: RouteDraft) => void }) {
  const { t } = useI18n();
  const selectedUpstream = upstreams.find((value) => value.id === draft.upstream_account_id);
  const supportedProtocols = providers.find((value) => value.id === selectedUpstream?.driver)?.protocols ?? ['openai', 'anthropic', 'generation'];
  const protocols = supportedProtocols.length > 0 ? supportedProtocols : ['openai', 'anthropic', 'generation'];
  return <>
    <label>{t('routes.publicModel')}<input value={draft.public_model} onChange={(event) => onChange({ ...draft, public_model: event.target.value })} /></label>
    <label>{t('routes.upstream')}<select value={draft.upstream_account_id} onChange={(event) => {
      const upstream_account_id = event.target.value;
      const upstream = upstreams.find((value) => value.id === upstream_account_id);
      const nextProtocols = providers.find((value) => value.id === upstream?.driver)?.protocols ?? protocols;
      onChange({ ...draft, upstream_account_id, protocol: nextProtocols.includes(draft.protocol) ? draft.protocol : (nextProtocols[0] ?? draft.protocol) });
    }}><option value="">{t('common.select')}</option>{upstreams.map((value) => <option key={value.id} value={value.id}>{value.name} · {value.driver}</option>)}</select></label>
    <label>{t('routes.upstreamModel')}<input value={draft.upstream_model} onChange={(event) => onChange({ ...draft, upstream_model: event.target.value })} /></label>
    <label>{t('routes.protocol')}<select value={draft.protocol} onChange={(event) => onChange({ ...draft, protocol: event.target.value })}>{protocols.map((protocol) => <option key={protocol} value={protocol}>{protocol === 'generation' ? t('routes.generation') : protocol === 'anthropic' ? 'Anthropic' : 'OpenAI'}</option>)}</select></label>
    <label>{t('routes.priority')}<input type="number" min={-1000000} max={1000000} value={draft.priority} onChange={(event) => onChange({ ...draft, priority: Number(event.target.value) })} /></label>
  </>;
}

function RouteWorkspace({ token, tenant, upstreams, providers }: { token: string; tenant: string; upstreams: UpstreamAccount[]; providers: ProviderType[] }) {
  const { locale, t } = useI18n();
  const [routes, setRoutes] = useState<ModelRouteView[]>([]);
  const [form, setForm] = useState<RouteDraft>(emptyRouteDraft);
  const [editing, setEditing] = useState<ModelRouteView>();
  const [editForm, setEditForm] = useState<RouteDraft>(emptyRouteDraft);
  const [busy, setBusy] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const load = async () => {
    if (!token || !tenant) { setRoutes([]); return; }
    try { setRoutes(await api<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(tenant)}`, token)); setError(''); }
    catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => { setEditing(undefined); setMessage(''); void load(); }, [token, tenant]);
  const scopedUpstreams = upstreams.filter((value) => !value.tenant_external_id || value.tenant_external_id === tenant);
  const canSubmit = (draft: RouteDraft) => Boolean(tenant && draft.public_model.trim() && draft.upstream_account_id && draft.upstream_model.trim());
  const beginEdit = (route: ModelRouteView) => {
    setEditing(route);
    setEditForm({ public_model: route.public_model, upstream_account_id: route.upstream_account_id, upstream_model: route.upstream_model, protocol: route.protocol, priority: route.priority });
    setMessage(''); setError('');
  };
  const saveEdit = async () => {
    if (!editing || !canSubmit(editForm)) return;
    setBusy(editing.id); setMessage(''); setError('');
    try {
      await api(`/internal/v1/model-routes/${editing.id}`, token, { method: 'PUT', body: JSON.stringify({ ...editForm, tenant_external_id: tenant, expected_updated_at: editing.updated_at }) });
      setEditing(undefined); setMessage(t('routes.updated')); await load();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  };
  const setEnabled = async (route: ModelRouteView, enabled: boolean) => {
    setBusy(route.id); setMessage(''); setError('');
    try {
      await api(`/internal/v1/model-routes/${route.id}`, token, { method: 'PATCH', body: JSON.stringify({ tenant_external_id: tenant, enabled, expected_updated_at: route.updated_at }) });
      setEditing(undefined); setMessage(t(enabled ? 'routes.enabled' : 'routes.disabled')); await load();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  };
  const remove = async (route: ModelRouteView) => {
    if (route.enabled || !window.confirm(t('routes.confirmDelete', { model: route.public_model }))) return;
    const query = new URLSearchParams({ tenant_external_id: tenant, expected_updated_at: String(route.updated_at) });
    setBusy(route.id); setMessage(''); setError('');
    try {
      await api(`/internal/v1/model-routes/${route.id}?${query}`, token, { method: 'DELETE' });
      setEditing(undefined); setMessage(t('routes.deleted')); await load();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  };
  return <><WriteScopeNotice tenant={tenant} /><section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('routes.title')}</h2><p className="muted">{t('routes.description')}</p></div><span>{formatNumber(routes.length, locale)}</span></div>{error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}<div className="table-scroll"><table><thead><tr><th>{t('routes.publicModel')}</th><th>{t('routes.upstream')}</th><th>{t('routes.upstreamModel')}</th><th>{t('routes.protocol')}</th><th>{t('routes.priority')}</th><th>{t('request.status')}</th><th>{t('routes.actions')}</th></tr></thead><tbody>{routes.map((route) => <tr key={route.id}><td><code>{route.public_model}</code></td><td>{scopedUpstreams.find((value) => value.id === route.upstream_account_id)?.name ?? route.upstream_account_id}</td><td><code>{route.upstream_model}</code></td><td>{route.protocol}</td><td>{formatNumber(route.priority, locale)}</td><td><span className={`status ${route.enabled ? 'ok' : 'pending'}`}>{route.enabled ? t('common.enabled') : t('common.disabled')}</span></td><td><div className="row-actions"><button type="button" className="secondary" disabled={busy === route.id || !tenant} onClick={() => beginEdit(route)}>{t('routes.edit')}</button><button type="button" className="secondary" disabled={busy === route.id || !tenant} onClick={() => void setEnabled(route, !route.enabled)}>{route.enabled ? t('routes.disable') : t('routes.enable')}</button><button type="button" className="danger" title={route.enabled ? t('routes.disableBeforeDelete') : undefined} disabled={busy === route.id || !tenant || route.enabled} onClick={() => void remove(route)}>{t('common.remove')}</button></div></td></tr>)}</tbody></table>{routes.length === 0 && <div className="empty">{t('routes.empty')}</div>}</div>
      {editing && <div className="inline-editor form-panel"><div className="panel-title"><h3>{t('routes.editTitle', { model: editing.public_model })}</h3><button type="button" className="secondary" onClick={() => setEditing(undefined)}>{t('common.cancel')}</button></div><RouteFields draft={editForm} upstreams={scopedUpstreams} providers={providers} onChange={setEditForm} /><button type="button" disabled={busy === editing.id || !canSubmit(editForm)} onClick={() => void saveEdit()}>{t('common.save')}</button></div>}
    </article>
    <article className="panel form-panel"><h2>{t('routes.createTitle')}</h2><RouteFields draft={form} upstreams={scopedUpstreams} providers={providers} onChange={setForm} /><button type="button" disabled={busy === 'create' || !canSubmit(form)} onClick={async () => { setBusy('create'); setMessage(''); setError(''); try { await api('/internal/v1/model-routes', token, { method: 'POST', body: JSON.stringify({ ...form, tenant_external_id: tenant }) }); setForm(emptyRouteDraft); setMessage(t('routes.created')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}>{t('routes.create')}</button></article>
  </section></>;
}

function CredentialWorkspace({ token, tenant, createSchema, policySchema }: { token: string; tenant: string; createSchema?: Record<string, unknown>; policySchema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const [values, setValues] = useState<KeyView[]>([]);
  const [editingPolicy, setEditingPolicy] = useState<string>();
  const [renaming, setRenaming] = useState<string>();
  const [aliasDraft, setAliasDraft] = useState('');
  const [limitSnapshots, setLimitSnapshots] = useState<Record<string, KeyLimitSnapshot>>({});
  const [granting, setGranting] = useState<string>();
  const [grant, setGrant] = useState({ amount: '', source: 'operator-console' });
  const [secret, setSecret] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const load = async () => {
    if (!token || !tenant) { setValues([]); return; }
    try { setValues(await api<KeyView[]>(`/internal/v1/keys${queryForTenant(tenant)}`, token)); setError(''); }
    catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => { setRenaming(undefined); setLimitSnapshots({}); void load(); }, [token, tenant]);
  return <><WriteScopeNotice tenant={tenant} />{secret && <OneTimeSecret value={secret} message={t('credentials.oneTimeSecret')} />}<section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('credentials.title')}</h2><p className="muted">{t('credentials.description')}</p></div><span>{formatNumber(values.length, locale)}</span></div>{error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}<div className="account-list">{values.length === 0 && <div className="empty">{t('credentials.empty')}</div>}{values.map((value) => <div className="managed-resource" key={value.key_id}><div className="managed-resource-header"><div><b>{value.alias}</b><small>{value.key_id}</small><span>{value.principal_external_id ?? t('common.unknownPrincipal')} · {formatCurrency(value.available_balance, value.currency, locale)}</span></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : value.status === 'revoked' ? 'bad' : 'pending'}`}>{enumLabel(t, 'status', value.status ?? 'active')}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span></div></div><div className="policy-chips"><span>RPM {formatNumber(value.policy.requests_per_minute, locale)}</span><span>TPM {formatNumber(value.policy.tokens_per_minute, locale)}</span><span>{t('self.concurrency')} {formatNumber(value.policy.max_concurrency, locale)}</span><span>{t('budget.daily')}: {value.policy.daily_budget === null ? '—' : formatCurrency(value.policy.daily_budget, value.currency, locale)}</span><span>{t('budget.weekly')}: {value.policy.weekly_budget === null ? '—' : formatCurrency(value.policy.weekly_budget, value.currency, locale)}</span><span>{t('budget.lifetime')}: {value.policy.lifetime_budget === null ? '—' : formatCurrency(value.policy.lifetime_budget, value.currency, locale)}</span><span>{t('self.allowedModels')}: {value.policy.allowed_models.length ? value.policy.allowed_models.join(', ') : t('credentials.noModelsAllowed')}</span></div><div className="row-actions"><button type="button" className="secondary" disabled={!tenant} onClick={() => { setRenaming(renaming === value.key_id ? undefined : value.key_id); setAliasDraft(value.alias); }}>{t('credentials.rename')}</button><button type="button" className="secondary" disabled={!tenant} onClick={async () => { try { const snapshot = await api<KeyLimitSnapshot>(`/internal/v1/keys/${value.key_id}/limits`, token); setLimitSnapshots((current) => ({ ...current, [value.key_id]: snapshot })); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('credentials.viewLimits')}</button><button type="button" className="secondary" disabled={!tenant || value.status === 'revoked'} onClick={async () => { try { const result = await api<{ key: string }>(`/internal/v1/keys/${value.key_id}/rotate`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } }); setSecret(result.key); setMessage(t('credentials.rotated', { alias: value.alias })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('credentials.rotate')}</button><button type="button" className="secondary" disabled={!tenant || value.status === 'revoked'} onClick={() => setEditingPolicy(editingPolicy === value.key_id ? undefined : value.key_id)}>{t('credentials.editPolicy')}</button><button type="button" className="secondary" disabled={!tenant || !value.account_id || value.status === 'revoked'} title={!value.account_id ? t('credentials.accountMissing') : undefined} onClick={() => setGranting(granting === value.key_id ? undefined : value.key_id)}>{t('credentials.grant')}</button>{value.status !== 'revoked' && <button type="button" className="secondary" disabled={!tenant} onClick={async () => { const nextStatus = value.status === 'active' ? 'suspended' : 'active'; try { await api(`/internal/v1/keys/${value.key_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); setMessage(t(nextStatus === 'active' ? 'credentials.resumed' : 'credentials.suspended', { alias: value.alias })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{value.status === 'active' ? t('credentials.suspend') : t('credentials.resume')}</button>}</div>
          {renaming === value.key_id && <div className="inline-editor form-panel"><h3>{t('credentials.renameFor', { alias: value.alias })}</h3><label>{t('schema.Credential alias')}<input value={aliasDraft} maxLength={200} onChange={(event) => setAliasDraft(event.target.value)} /></label><button type="button" disabled={!aliasDraft.trim()} onClick={async () => { try { await api(`/internal/v1/keys/${value.key_id}/alias`, token, { method: 'PATCH', body: JSON.stringify({ alias: aliasDraft }) }); setRenaming(undefined); setMessage(t('credentials.renamed', { alias: aliasDraft.trim() })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('common.save')}</button></div>}
          {limitSnapshots[value.key_id] && <LimitSnapshot value={limitSnapshots[value.key_id]} />}
          {editingPolicy === value.key_id && policySchema && <div className="inline-editor form-panel"><h3>{t('credentials.policyFor', { alias: value.alias })}</h3><Form key={`${value.key_id}-${locale}`} schema={localizeSchema(policySchema as RJSFSchema, locale)} formData={value.policy} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { try { await api(`/internal/v1/keys/${value.key_id}/policy`, token, { method: 'PUT', body: JSON.stringify(formData) }); setEditingPolicy(undefined); setMessage(t('credentials.policySaved')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant}>{t('common.save')}</button></Form></div>}
          {granting === value.key_id && value.account_id && <div className="inline-editor form-panel"><h3>{t('credentials.grantFor', { alias: value.alias })}</h3><label>{t('credentials.grantAmount')}<input inputMode="decimal" value={grant.amount} onChange={(event) => setGrant({ ...grant, amount: event.target.value })} /></label><label>{t('credentials.grantSource')}<input value={grant.source} onChange={(event) => setGrant({ ...grant, source: event.target.value })} /></label><button type="button" disabled={!grant.amount || !grant.source.trim()} onClick={async () => { try { await api(`/internal/v1/accounts/${value.account_id}/grants`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify(grant) }); setGranting(undefined); setGrant({ amount: '', source: 'operator-console' }); setMessage(t('credentials.granted')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('credentials.confirmGrant')}</button></div>}
        </div>)}</div></article>
    <article className="panel form-panel"><h2>{t('credentials.createTitle')}</h2>{createSchema ? <Form key={`${tenant}-${locale}`} schema={localizeSchema(createSchema as RJSFSchema, locale)} uiSchema={{ tenant_external_id: { 'ui:widget': 'hidden' }, policy: { allowed_models: { 'ui:options': { orderable: false } } } }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { const created = await api<{ key: string }>('/internal/v1/keys', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant }) }); setSecret(created.key); setMessage(t('credentials.created')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant}>{t('credentials.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}</article>
  </section></>;
}

function ServiceCredentialWorkspace({ token, tenant, schema }: { token: string; tenant: string; schema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const [values, setValues] = useState<ServiceTokenView[]>([]);
  const [secret, setSecret] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const load = async () => {
    if (!token) { setValues([]); return; }
    try {
      const all = await api<ServiceTokenView[]>('/internal/v1/service-tokens', token);
      setValues(tenant ? all.filter((value) => value.tenant_external_id === tenant) : all); setError('');
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => { void load(); }, [token, tenant]);
  return <>{!tenant && <div className="notice warning" role="status">{t('services.allTenantNotice')}</div>}{secret && <OneTimeSecret value={secret} message={t('services.oneTimeSecret')} />}<section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('services.title')}</h2><p className="muted">{t('services.description')}</p></div><span>{formatNumber(values.length, locale)}</span></div>{error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}<div className="account-list">{values.length === 0 && <div className="empty">{t('services.empty')}</div>}{values.map((value) => <div className="managed-resource" key={value.service_id}><div className="managed-resource-header"><div><b>{value.name}</b><small>{value.service_id}</small><span>{value.tenant_external_id ?? t('services.globalScope')} · {value.scopes.join(' · ')}</span></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : value.status === 'revoked' ? 'bad' : 'pending'}`}>{enumLabel(t, 'status', value.status ?? 'active')}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span></div></div><div className="row-actions"><button type="button" className="secondary" disabled={value.status === 'revoked'} onClick={async () => { try { const result = await api<{ token: string }>(`/internal/v1/service-tokens/${value.service_id}/rotate`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } }); setSecret(result.token); setMessage(t('services.rotated', { name: value.name })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('services.rotate')}</button>{value.status !== 'revoked' && <button type="button" className="secondary" onClick={async () => { const nextStatus = value.status === 'active' ? 'suspended' : 'active'; try { await api(`/internal/v1/service-tokens/${value.service_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); setMessage(t(nextStatus === 'active' ? 'services.resumed' : 'services.suspended', { name: value.name })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{value.status === 'active' ? t('services.suspend') : t('services.resume')}</button>}</div></div>)}</div></article>
    <article className="panel form-panel"><h2>{t('services.createTitle')}</h2>{schema ? <Form key={`${tenant}-${locale}`} schema={localizeSchema(schema as RJSFSchema, locale)} uiSchema={{ tenant_external_id: { 'ui:widget': 'hidden' } }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { const created = await api<{ token: string }>('/internal/v1/service-tokens', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant }) }); setSecret(created.token); setMessage(t('services.created')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant}>{t('services.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}</article>
  </section></>;
}

function Plugins({ values }: { values: PluginManifest[] }) {
  const { locale, t } = useI18n();
  return <article className="panel"><div className="panel-title"><h2>{t('plugins.title')}</h2><span>{t('plugins.runtime')}</span></div><div className="account-list">{values.length === 0 && <div className="empty">{t('plugins.empty')}</div>}{values.map((value) => <div className="account" key={value.id}><div><b>{value.id}</b><span>v{value.version} · WIT {value.wit_version} · {t('plugins.providerCount', { count: formatNumber((value.contributions.providers ?? []).length, locale) })}</span></div><span className="pill">{value.contributions.traffic_policy ? t('plugins.trafficPolicy') : t('plugins.provider')}</span></div>)}</div></article>;
}

function RequestDrawer({ detail, onClose }: { detail: RequestDetail; onClose: () => void }) {
  const { t } = useI18n();
  return <DrawerFrame title={detail.model} eyebrow={t('request.operatorDiagnosis')} onClose={onClose}><p className="muted break-anywhere">{detail.request_id} · {detail.status_code ?? t('common.running')} · {detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</p><h3>{t('request.error')}</h3><pre>{detail.error_code ?? t('common.none')}</pre><h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre><h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre></DrawerFrame>;
}
