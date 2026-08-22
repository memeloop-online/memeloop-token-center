import RjsfForm, { type FormProps } from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type RefObject } from 'react';
import { ApiError, api, streamSse } from '../api';
import { DrawerFrame, RequestTable, Shell } from '../components';
import { formatCurrency, formatNumber } from '../format';
import { localizeSchema, useI18n } from '../i18n';
import { LimitSnapshot } from '../LimitSnapshot';
import { schemaFormFields, schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator as validator } from '../safeValidator';
import type {
  ConfigurationSchemas, CredentialRoutingView, GenerationPriceView, GroupView, KeyLimitSnapshot, KeyView, ModelPriceSyncResult,
  ModelPriceUsageSummary, ModelPriceView, ModelRouteView,
  PluginManifest, ProviderType, RequestDetail, RequestEvent, RequestView,
  ServiceTokenView, TenantView, UpstreamAccount, UpstreamHealth, UsageAnalysisSessionBucket,
} from '../types';
import './operator.css';
import { GroupManager, useGroups } from './GroupManager';
import { GenerationWorkspace } from './GenerationWorkspace';
import { MultiCombobox, type ComboboxOption } from './MultiCombobox';
import { UpstreamModelCombobox } from './UpstreamModelCombobox';
import { Plugins } from './Plugins';
import { SessionMonitor, type SessionFocus, type SessionStreamState } from './SessionMonitor';
import { enqueueSessionEventKey } from './sessionRefresh';
import { UsageAnalysis } from './UsageAnalysis';
import { directCredentialSchema, supportsDirectConnection } from './providerConnectionMethods';

type Tab = 'traffic' | 'usage' | 'generations' | 'providers' | 'routes' | 'pricing' | 'credentials' | 'services' | 'plugins';
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
const tabIds: Tab[] = ['traffic', 'usage', 'generations', 'providers', 'routes', 'pricing', 'credentials', 'services', 'plugins'];
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
  const [trafficMode, setTrafficMode] = useState<'requests' | 'sessions'>('requests');
  const [sessionRevision, setSessionRevision] = useState(0);
  const [sessionFocus, setSessionFocus] = useState<SessionFocus>();
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
  const [streamState, setStreamState] = useState<SessionStreamState>('idle');
  const requestEventCursor = useRef<RequestEventCursor | undefined>(undefined);
  const requestEventScope = useRef<RequestEventScope | undefined>(undefined);
  const liveRequestEvents = useRef(new Map<string, RequestEvent>());
  const sessionEventKeyIds = useRef(new Set<string>());

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
      sessionEventKeyIds.current.clear();
      requestEventScope.current = { credential: token, tenant, filters: requestFilters };
      setStreamError('');
    }
    if (!token || tab !== 'traffic' || (trafficMode === 'requests' && filtersActive(requestFilters))) {
      setStreamError('');
      setStreamState('idle');
      return;
    }
    const activeScope = requestEventScope.current;
    const controller = new AbortController();
    let connectedOnce = false;
    const connect = async () => {
      while (!controller.signal.aborted) {
        setStreamState(connectedOnce ? 'reconnecting' : 'connecting');
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
              setStreamState('live');
              enqueueSessionEventKey(sessionEventKeyIds.current, event.key_id);
              setSessionRevision((revision) => revision + 1);
              setRequests((current) => {
                const previous = current.find((request) => request.request_id === event.request_id);
                const next = requestViewFromEvent(event, previous);
                return [next, ...current.filter((request) => request.request_id !== event.request_id)]
                  .sort((left, right) => right.created_at - left.created_at).slice(0, 100);
              });
            },
            () => {
              if (controller.signal.aborted || requestEventScope.current !== activeScope) return;
              connectedOnce = true;
              setStreamError('');
              setStreamState('live');
            },
          );
          if (!controller.signal.aborted) {
            // A clean SSE EOF is a normal reconnect boundary (for example an
            // ingress stream lifetime). Keep the explicit reconnecting state,
            // but reserve the error notice for failed HTTP or parsing paths.
            setStreamError('');
            setStreamState('reconnecting');
          }
        } catch (reason) {
          if (!controller.signal.aborted) {
            setStreamError(messageOf(reason, t('traffic.streamDisconnected')));
            setStreamState('reconnecting');
          }
        }
        await waitForReconnect(controller.signal, 1000);
      }
    };
    void connect();
    return () => controller.abort();
  }, [token, tab, trafficMode, tenant, requestFilters]);

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
      {tab === 'traffic' && <Traffic token={token} tenant={tenant} mode={trafficMode} onModeChange={setTrafficMode} sessionRevision={sessionRevision} sessionEventKeyIds={sessionEventKeyIds} sessionFocus={sessionFocus} streamState={streamState} requests={requests} upstreams={upstreams} filters={requestFilters} loading={requestsLoading} hasOlder={hasOlderRequests} onApply={(filters) => { setRequestFilters(filters); void loadRequests(filters); }} onClear={() => { setRequestFilters(emptyRequestFilters); void loadRequests(emptyRequestFilters); }} onLoadOlder={() => void loadRequests(requestFilters, true)} onSelect={selectRequest} />}
      {tab === 'usage' && <UsageAnalysis token={token} tenant={tenant} upstreams={upstreams} onOpenSession={(session: UsageAnalysisSessionBucket) => { setSessionFocus({ sessionId: session.id, keyId: session.key_id, revision: Date.now() }); setTrafficMode('sessions'); setTab('traffic'); }} />}
      {tab === 'generations' && <GenerationWorkspace token={token} tenant={tenant} />}
      {tab === 'providers' && <UpstreamProviders token={token} tenant={tenant} providers={providers} values={upstreams} onChanged={refresh} />}
      {tab === 'routes' && <RouteWorkspace token={token} tenant={tenant} upstreams={upstreams} providers={providers} />}
      {tab === 'pricing' && <Pricing token={token} tenant={tenant} schemas={schemas} />}
      {tab === 'credentials' && <CredentialWorkspace token={token} tenant={tenant} createSchema={schemas?.key_create} policySchema={schemas?.key_policy} />}
      {tab === 'services' && <ServiceCredentialWorkspace token={token} tenant={tenant} schema={schemas?.service_token} />}
      {tab === 'plugins' && <Plugins token={token} tenant={tenant} values={plugins} />}
    </section>
    {detail && <RequestDrawer detail={detail} onClose={() => setDetail(undefined)} />}
  </Shell>;
}

function Traffic({ token, tenant, mode, onModeChange, sessionRevision, sessionEventKeyIds, sessionFocus, streamState, requests, upstreams, filters, loading, hasOlder, onApply, onClear, onLoadOlder, onSelect }: {
  token: string;
  tenant: string;
  mode: 'requests' | 'sessions';
  onModeChange: (mode: 'requests' | 'sessions') => void;
  sessionRevision: number;
  sessionEventKeyIds: RefObject<Set<string>>;
  sessionFocus?: SessionFocus;
  streamState: SessionStreamState;
  requests: RequestView[];
  upstreams: UpstreamAccount[];
  filters: RequestFilters;
  loading: boolean;
  hasOlder: boolean;
  onApply: (filters: RequestFilters) => void;
  onClear: () => void;
  onLoadOlder: () => void;
  onSelect: (request: RequestView) => Promise<void>;
}) {
  const { t } = useI18n();
  const [draft, setDraft] = useState(filters);
  useEffect(() => setDraft(filters), [filters]);

  return <article className="panel"><div className="panel-title traffic-heading"><div><h2>{mode === 'sessions' ? t('sessions.recent') : filtersActive(filters) ? t('traffic.filtered') : t('traffic.live')}</h2><span>{mode === 'sessions' ? t('sessions.monitorHint') : filtersActive(filters) ? t('traffic.filteredHint') : t('traffic.liveHint')}</span></div><div className="segmented" role="group" aria-label={t('sessions.monitorMode')}><button type="button" className={mode === 'requests' ? 'active' : ''} aria-pressed={mode === 'requests'} onClick={() => onModeChange('requests')}>{t('sessions.requestsMode')}</button><button type="button" className={mode === 'sessions' ? 'active' : ''} aria-pressed={mode === 'sessions'} onClick={() => onModeChange('sessions')}>{t('sessions.sessionsMode')}</button></div></div>
    {mode === 'requests' ? <>
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
    </> : <SessionMonitor token={token} tenant={tenant} revision={sessionRevision} eventKeyIds={sessionEventKeyIds} focus={sessionFocus} streamState={streamState} onSelectRequest={onSelect} />}
  </article>;
}
function UpstreamProviders({ token, tenant, providers, values, onChanged }: { token: string; tenant: string; providers: ProviderType[]; values: UpstreamAccount[]; onChanged: () => Promise<void> }) {
  const { locale, t } = useI18n();
  const [method, setMethod] = useState<'direct' | 'authorization'>('direct');
  const [driver, setDriver] = useState('');
  const [rotating, setRotating] = useState<UpstreamAccount>();
  const [editing, setEditing] = useState<UpstreamAccount>();
  const [reauthorizing, setReauthorizing] = useState<UpstreamAccount>();
  const [busy, setBusy] = useState('');
  const [health, setHealth] = useState<Record<string, UpstreamHealth>>({});
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const providerGroups = useGroups('provider', token, tenant);
  const directProviders = providers.filter(supportsDirectConnection);
  const provider = directProviders.find((value) => value.id === driver) ?? directProviders[0];
  const schema = useMemo<RJSFSchema | undefined>(() => {
    if (!provider) return undefined;
    const config = structuredClone(provider.config_schema) as { properties?: Record<string, unknown> };
    if (provider.id === 'http-json' && config.properties) {
      delete config.properties.oauth;
      delete config.properties.timeout_seconds;
    }
    const credential = directCredentialSchema(provider.credential_schema) as { oneOf?: Array<Record<string, unknown>> } | undefined;
    if (!credential) return undefined;
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
  const uiSchema = {
    driver: { 'ui:widget': 'hidden' },
    config: {
      oauth: { 'ui:widget': 'hidden' },
      timeout_seconds: { 'ui:widget': 'hidden' },
      ...(provider?.id === 'comfyui' ? {
        workflow_template: { 'ui:field': 'JsonObject' },
        parameter_schema: { 'ui:field': 'JsonObject' },
      } : {}),
    },
  };
  useEffect(() => { setRotating(undefined); setEditing(undefined); setReauthorizing(undefined); setHealth({}); }, [tenant]);

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

  async function disconnectOAuth(value: UpstreamAccount) {
    if (!canManage(value) || !window.confirm(t('providers.confirmDisconnect', { name: value.name }))) return;
    setBusy(`disconnect-${value.id}`);
    setError(''); setMessage('');
    try {
      await api(`/internal/v1/upstreams/${value.id}/oauth/disconnect`, token, {
        method: 'POST',
        body: JSON.stringify({ tenant_external_id: tenant, expected_updated_at: value.updated_at }),
      });
      setMessage(t('providers.disconnected', { name: value.name }));
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
      {error && <div className="notice error" role="alert">{error}</div>}{providerGroups.error && <div className="notice error" role="alert">{providerGroups.error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      <div className="account-list">{values.length === 0 && <div className="empty">{t('providers.empty')}</div>}{values.map((value) => {
        const currentHealth = health[value.id];
        const manageable = canManage(value);
        const memberships = providerGroups.groups.filter((group) => group.member_ids.includes(value.id));
        return <div className="account provider-account" key={value.id}><div className="account-main"><b>{value.name}</b><span>{value.driver} · {t('providers.method')}: {enumLabel(t, 'auth', value.connection_method)}{value.tenant_external_id ? ` · ${value.tenant_external_id}` : ''}</span>{memberships.length > 0 && <div className="table-chip-list provider-group-summary" aria-label={t('groups.provider.title')}>{memberships.map((group) => <span key={group.id}>{group.name}</span>)}</div>}<small>{value.id}</small>{value.credential_expires_at && <small>{t('providers.expires')}: {new Date(value.credential_expires_at).toLocaleString(locale)}</small>}{currentHealth && <small className={`status ${currentHealth.status === 'healthy' ? 'ok' : 'pending'}`}>{currentHealth.status === 'healthy' ? t('providers.healthy') : t('providers.unhealthy')}{currentHealth.upstream_status ? ` · HTTP ${formatNumber(currentHealth.upstream_status, locale)}` : ''}{currentHealth.latency_ms !== undefined ? ` · ${formatNumber(currentHealth.latency_ms, locale, 2)} ms` : ''}</small>}</div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : 'pending'}`}>{enumLabel(t, 'status', value.status)}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span><span className="pill">{t('providers.routes', { count: formatNumber(value.route_count, locale) })}</span><div className="row-actions"><button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setEditing(value)}>{t('providers.edit')}</button><button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void checkHealth(value)}>{t('providers.health')}</button>{value.can_refresh && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void refreshOAuth(value)}>{t('providers.refreshAuthorization')}</button>}{value.can_reauthorize && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setReauthorizing(value)}>{t('providers.reauthorize')}</button>}{value.auth_kind === 'oauth' && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void disconnectOAuth(value)}>{t('providers.disconnect')}</button>}{value.can_rotate && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setRotating(value)}>{t('providers.rotateCredential')}</button>}<button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void setStatus(value, value.status === 'active' ? 'disabled' : 'active')}>{value.status === 'active' ? t('providers.disable') : t('providers.enable')}</button><button type="button" className="danger" title={value.status !== 'disabled' ? t('providers.disableBeforeDelete') : value.route_count > 0 ? t('providers.removeRoutesFirst') : undefined} disabled={!manageable || Boolean(busy) || value.status !== 'disabled' || value.route_count > 0} onClick={() => void remove(value)}>{t('common.remove')}</button></div></div></div>;
      })}</div>
      {editing && editSchema && <div className="inline-editor"><div className="panel-title"><h3>{t('providers.editFor', { name: editing.name })}</h3><button type="button" className="secondary" onClick={() => setEditing(undefined)}>{t('common.cancel')}</button></div><Form key={`${editing.id}-${locale}`} schema={editSchema} uiSchema={{ config: { oauth: { 'ui:disabled': true } } }} formData={{ name: editing.name, config: editing.config }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!formData) return; setBusy(`edit-${editing.id}`); try { await api(`/internal/v1/upstreams/${editing.id}`, token, { method: 'PUT', body: JSON.stringify({ ...formData, tenant_external_id: tenant, expected_updated_at: editing.updated_at }) }); setEditing(undefined); setMessage(t('providers.updated', { name: editing.name })); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}><button type="submit" disabled={!canManage(editing) || Boolean(busy)}>{t('common.save')}</button></Form></div>}
      {rotating && rotateProvider && <div className="inline-editor"><div className="panel-title"><h3>{t('providers.rotateFor', { name: rotating.name })}</h3><button type="button" className="secondary" onClick={() => setRotating(undefined)}>{t('common.cancel')}</button></div><Form key={`${rotating.id}-${locale}`} schema={localizeSchema(rotateProvider.credential_schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { setBusy(`rotate-${rotating.id}`); try { await api(`/internal/v1/upstreams/${rotating.id}/credential`, token, { method: 'PUT', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify({ credential: formData }) }); setRotating(undefined); setMessage(t('providers.rotated', { name: rotating.name })); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}><button type="submit" disabled={!canManage(rotating) || Boolean(busy)}>{t('providers.confirmRotate')}</button></Form></div>}
    </article>
    <article className="panel form-panel provider-onboarding">{reauthorizing ? <>
      <div className="panel-title"><h2>{t('providers.reauthorizeFor', { name: reauthorizing.name })}</h2><button type="button" className="secondary" onClick={() => setReauthorizing(undefined)}>{t('common.cancel')}</button></div>
      <AuthorizationConnection key={`reauthorize-${reauthorizing.id}`} token={token} tenant={tenant} providers={providers} existing={reauthorizing} onChanged={async () => { setReauthorizing(undefined); setMessage(t('providers.reauthorized', { name: reauthorizing.name })); await onChanged(); }} />
    </> : <><h2>{t('providers.add')}</h2>
      <div className="segmented" role="group" aria-label={t('providers.method')}><button type="button" aria-pressed={method === 'direct'} className={method === 'direct' ? 'active' : ''} onClick={() => setMethod('direct')}>{t('providers.direct')}</button><button type="button" aria-pressed={method === 'authorization'} className={method === 'authorization' ? 'active' : ''} onClick={() => setMethod('authorization')}>{t('providers.oauth')}</button></div>
      {method === 'direct' ? <>
        <label>{t('providers.provider')}<select value={provider?.id ?? ''} onChange={(event) => setDriver(event.target.value)}>{directProviders.map((value) => <option key={value.id} value={value.id}>{value.display_name} · {value.source}</option>)}</select></label>
        {schema ? <Form key={`${provider.id}-${locale}`} schema={schema} uiSchema={uiSchema} fields={schemaFormFields} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { setError(''); await api('/internal/v1/upstreams', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant }) }); setMessage(t('providers.created')); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant || !token}>{t('providers.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
      </> : <AuthorizationConnection token={token} tenant={tenant} providers={providers} onChanged={onChanged} />}</>}
    </article>
  </section></>;
}

function AuthorizationConnection({ token, tenant, providers, existing, onChanged }: { token: string; tenant: string; providers: ProviderType[]; existing?: UpstreamAccount; onChanged: () => Promise<void> }) {
  const { locale, t } = useI18n();
  const oauthProviders = providers.filter((provider) => provider.oauth_adapter);
  const existingOAuthProvider = oauthProviders.find((provider) => provider.id === existing?.driver);
  const initialProvider = existingOAuthProvider ?? oauthProviders[0];
  const [providerChoice, setProviderChoice] = useState(initialProvider?.id ?? '');
  const selectedProvider = oauthProviders.find((provider) => provider.id === providerChoice);
  const [name, setName] = useState(existing?.name ?? (initialProvider ? `${initialProvider.id}-primary` : ''));
  const [session, setSession] = useState<{ login_url?: string; verification_url?: string; user_code?: string; session_token: string; expires_at?: number; poll_after_seconds?: number }>();
  const [manualCode, setManualCode] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const reset = () => { setSession(undefined); setManualCode(''); setMessage(''); setError(''); };
  useEffect(() => { setSession(undefined); setManualCode(''); setMessage(''); setError(''); }, [tenant]);
  const start = async (providerConfig?: unknown) => {
    if (!tenant || !selectedProvider) return;
    try {
      const target = existing ? { upstream_account_id: existing.id } : {};
      const flow = selectedProvider.oauth_adapter?.flow_kind;
      if (flow === 'openai_device') {
        setSession(await api('/internal/v1/oauth/codex/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, ...target }) }));
      } else if (flow === 'claude_manual_pkce') {
        setSession(await api('/internal/v1/oauth/claude/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, ...target }) }));
      } else if (flow === 'github_device_copilot') {
        setSession(await api('/internal/v1/oauth/copilot/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, ...target }) }));
      } else if (flow === 'cursor_pkce' && selectedProvider.source === 'builtin') {
        setSession(await api('/internal/v1/oauth/cursor/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider_driver: selectedProvider.id, provider_config: existing?.config ?? { base_url: 'https://api2.cursor.sh', network_scope: 'public' }, ...target }) }));
      } else {
        setSession(await api('/internal/v1/oauth/provider-adapter/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider_driver: selectedProvider.id, provider_config: existing?.config ?? providerConfig, ...target }) }));
      }
      setMessage(''); setError('');
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  const poll = async () => {
    if (!session) return;
    if (!selectedProvider) return;
    const flow = selectedProvider.oauth_adapter?.flow_kind;
    const path = flow === 'openai_device' ? '/internal/v1/oauth/codex/poll'
      : flow === 'github_device_copilot' ? '/internal/v1/oauth/copilot/poll'
      : flow === 'cursor_pkce' && selectedProvider.source === 'builtin' ? '/internal/v1/oauth/cursor/poll'
      : '/internal/v1/oauth/provider-adapter/poll';
    try {
      const result = await api<UpstreamAccount | { status: string; message?: string }>(path, token, { method: 'POST', body: JSON.stringify({ session_token: session.session_token }) });
      if ('id' in result) { setMessage(t(existing ? 'providers.reauthorized' : 'providers.ready', existing ? { name: result.name } : { id: result.id })); setSession(undefined); await onChanged(); }
      else setMessage(result.message ?? t('providers.waiting'));
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  const complete = async () => {
    if (!session || selectedProvider?.oauth_adapter?.flow_kind !== 'claude_manual_pkce') return;
    try {
      const result = await api<UpstreamAccount>('/internal/v1/oauth/claude/complete', token, { method: 'POST', body: JSON.stringify({ session_token: session.session_token, authorization_code: manualCode }) });
      setMessage(t(existing ? 'providers.reauthorized' : 'providers.ready', existing ? { name: result.name } : { id: result.id }));
      setSession(undefined); setManualCode(''); await onChanged();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  return <div className="authorization-form"><p className="muted">{t('providers.oauthSecurity')}</p>
    {error && <div className="notice error" role="alert">{error}</div>}
    {oauthProviders.length === 0 ? <div className="empty">{t('providers.noAdapter')}</div> : <>
    <label>{t('providers.provider')}<select disabled={Boolean(existing)} value={providerChoice} onChange={(event) => { const next = event.target.value; setProviderChoice(next); setName(`${next}-primary`); reset(); }}>{oauthProviders.map((value) => <option key={value.id} value={value.id}>{value.display_name}</option>)}</select></label>
    <label>{t('providers.name')}<input readOnly={Boolean(existing)} value={name} onChange={(event) => setName(event.target.value)} /></label>
    {selectedProvider && selectedProvider.source !== 'builtin' && !session ? <Form key={`${selectedProvider.id}-${locale}`} schema={localizeSchema(selectedProvider.config_schema as RJSFSchema, locale)} formData={existing?.config} readonly={Boolean(existing)} validator={validator} templates={schemaFormTemplates} onSubmit={({ formData }) => void start(formData)}><button type="submit" disabled={!tenant}>{t('common.startLogin')}</button></Form> : <div className="button-row"><button type="button" onClick={() => void start()} disabled={!tenant || Boolean(session)}>{t('common.startLogin')}</button>{session && <><a className="button secondary" href={session.verification_url ?? session.login_url} target="_blank" rel="noreferrer">{t('common.openAuthorization')}</a>{selectedProvider?.oauth_adapter?.flow_kind !== 'claude_manual_pkce' && <button type="button" onClick={() => void poll()}>{t('common.checkAuthorization')}</button>}</>}</div>}
    {session && selectedProvider?.oauth_adapter?.flow_kind === 'claude_manual_pkce' && <div className="manual-authorization"><label>{t('providers.manualCode')}<input value={manualCode} onChange={(event) => setManualCode(event.target.value)} placeholder={t('providers.manualCodeHint')} /></label><button type="button" disabled={!manualCode.includes('#')} onClick={() => void complete()}>{t('providers.completeAuthorization')}</button></div>}
    {session?.user_code && <div className="device-authorization" role="status"><p>{t('providers.codexSecurity')}</p><b>{t('providers.deviceCode')}</b><code>{session.user_code}</code></div>}
    {message && <div className="notice success" role="status">{message}</div>}
    </>}
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
  const [displayCurrency, setDisplayCurrency] = useState('USD');
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const scope = queryForTenant(tenant);
  const load = async (requestedCurrency = displayCurrency) => {
    const sequence = ++loadSequence.current;
    if (!token) return;
    const results = await Promise.allSettled([
      api<ModelPriceView[]>(`/internal/v1/model-prices?currency=${encodeURIComponent(requestedCurrency)}`, token),
      api<ModelPriceUsageSummary>(`/internal/v1/model-prices/usage-summary${scope}`, token),
      api<GenerationPriceView[]>(`/internal/v1/generation-prices?currency=${encodeURIComponent(requestedCurrency)}`, token),
    ]);
    if (sequence !== loadSequence.current) return;
    const [nextPrices, nextUsage, nextGenerationPrices] = results;
    if (nextPrices.status === 'fulfilled') setPrices(nextPrices.value);
    if (nextUsage.status === 'fulfilled') setUsage(nextUsage.value);
    if (nextGenerationPrices.status === 'fulfilled') setGenerationPrices(nextGenerationPrices.value);
    const failures = results.filter((result) => result.status === 'rejected');
    setError(failures.length ? t('pricing.partialLoad', { count: formatNumber(failures.length, locale) }) : '');
  };
  useEffect(() => { void load(displayCurrency); }, [token, tenant, displayCurrency]);
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
      const result = await api<ModelPriceSyncResult>('/internal/v1/model-prices/sync', token, { method: 'POST', body: JSON.stringify({ models: usage.models.map((value) => value.model), currency: displayCurrency, tenant_external_id: tenant }) });
      setSyncResult(result); setPrices(result.prices); setMessage(t('pricing.synced', { count: formatNumber(result.imported, locale) }));
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setSyncing(false); }
  };
  return <div className="pricing-page"><WriteScopeNotice tenant={tenant} />
    <article className="panel pricing-overview"><div className="panel-title"><div><h2>{t('pricing.title')}</h2><p className="muted">{t('pricing.description')}</p></div><div className="pricing-heading-actions"><label>{t('pricing.viewCurrency')}<select aria-label={t('pricing.viewCurrency')} value={displayCurrency} onChange={(event) => { const next = event.target.value; setDisplayCurrency(next); setCurrency(next); }}><option value="USD">USD</option><option value="CNY">CNY</option></select></label><button type="button" onClick={() => void sync()} disabled={!tenant || syncing}>{syncing ? t('pricing.syncing') : t('pricing.sync')}</button></div></div>
      <div className="pricing-summary"><span>{t('pricing.usedModels', { count: formatNumber(usage.models.length, locale) })}</span><span>{t('pricing.saved', { count: formatNumber(prices.length, locale) })}</span><span>{t('pricing.sourceOrder')}: models.dev → LiteLLM → OpenRouter</span></div>
      {error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      {syncResult && <><div className="source-status">{syncResult.sourceResults.map((source) => <div className={`source-card ${source.error ? 'failed' : 'healthy'}`} key={source.source}><b>{source.source}</b><span>{source.error ? t('pricing.sourceFailed') : t('pricing.sourceHealthy', { count: formatNumber(source.models, locale) })}</span>{source.error && <small>{source.error}</small>}</div>)}</div><div className="notice success"><b>{t('pricing.result')}</b> · {t('pricing.imported', { count: formatNumber(syncResult.imported, locale) })} · {t('pricing.candidates', { count: formatNumber(syncResult.candidates.length, locale) })} · {t('pricing.unmatched', { count: formatNumber(syncResult.unmatched.length, locale) })} · {t('pricing.preserved', { count: formatNumber(syncResult.preserved.length, locale) })}</div>
        {(syncResult.candidates.length > 0 || syncResult.unmatched.length > 0) && <div className="sync-details"><h3>{t('pricing.candidateDetails')}</h3>{syncResult.candidates.map((candidate) => <details key={candidate.model}><summary><code>{candidate.model}</code><span>{t('pricing.candidateCount', { count: formatNumber(candidate.candidates.length, locale) })}</span></summary><div className="candidate-list">{candidate.candidates.map((match) => <div key={`${match.source}-${match.sourceModelId}-${match.serviceTier}`}><b>{match.sourceModelId}</b><span>{match.source} · {match.serviceTier} · {match.reason}</span><code>{t('pricing.input')}: {formatCurrency(match.inputPerMillion, displayCurrency, locale)} · {t('pricing.output')}: {formatCurrency(match.outputPerMillion, displayCurrency, locale)}</code></div>)}</div></details>)}{syncResult.unmatched.length > 0 && <details><summary>{t('pricing.unmatchedModels')}</summary><div className="model-name-list">{syncResult.unmatched.map((name) => <code key={name}>{name}</code>)}</div></details>}</div>}
      </>}
      <div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.calls')}</th><th>{t('pricing.serviceTier')}</th><th>{t('pricing.input')}</th><th>{t('pricing.cachedInput')}</th><th>{t('pricing.cacheWrite')}</th><th>{t('pricing.output')}</th><th>{t('pricing.source')}</th><th>{t('pricing.updated')}</th></tr></thead><tbody>{rows.map((row) => <tr key={`${row.model}-${row.tier?.service_tier ?? 'missing'}`}><td><code>{row.model}</code></td><td>{row.usage ? formatNumber(row.usage.calls, locale) : ''}</td><td>{row.tier?.service_tier ?? '—'}</td><td>{row.tier ? formatCurrency(row.tier.input_per_million, displayCurrency, locale) : '—'}</td><td>{row.tier ? <>{formatCurrency(row.tier.cached_input_per_million, displayCurrency, locale)}{row.tier.cache_price_estimated && <small className="muted"> {t('pricing.estimated')}</small>}</> : '—'}</td><td>{row.tier ? <>{formatCurrency(row.tier.cache_write_per_million, displayCurrency, locale)}{row.tier.cache_price_estimated && <small className="muted"> {t('pricing.estimated')}</small>}</> : '—'}</td><td>{row.tier ? formatCurrency(row.tier.output_per_million, displayCurrency, locale) : '—'}</td><td>{row.tier ? <span className={`pill source-${row.tier.source.replace('.', '-')}`}>{row.tier.source}</span> : <span className="status pending">{t('pricing.missing')}</span>}</td><td>{row.tier ? new Date(row.tier.updated_at).toLocaleString(locale) : '—'}</td></tr>)}</tbody></table>{rows.length === 0 && <div className="empty">{t('pricing.noPricesForCurrency', { currency: displayCurrency })}</div>}</div>
    </article>
    <article className="panel"><div className="panel-title"><h2>{t('pricing.generationPrices')}</h2><span>{formatNumber(generationPrices.length, locale)}</span></div><div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.currency')}</th><th>{t('self.units')}</th><th>{t('pricing.unitPrice')}</th></tr></thead><tbody>{generationPrices.map((price) => <tr key={`${price.currency}-${price.model}`}><td><code>{price.model}</code></td><td>{price.currency}</td><td>{enumLabel(t, 'billingUnit', price.billing_unit)}</td><td>{formatCurrency(price.price_per_unit, price.currency, locale)}</td></tr>)}</tbody></table>{generationPrices.length === 0 && <div className="empty">{t('pricing.noGenerationPrices')}</div>}</div></article>
    <details className="panel manual-pricing"><summary><span><b>{t('pricing.manual')}</b><small>{t('pricing.manualHint')}</small></span><span>＋</span></summary><div className="manual-pricing-body form-panel"><label>{t('pricing.type')}<select value={kind} onChange={(event) => setKind(event.target.value as typeof kind)}><option value="token">{t('pricing.tokenModel')}</option><option value="generation">{t('pricing.generationModel')}</option></select></label><label>{t('pricing.model')}<input value={model} onChange={(event) => setModel(event.target.value)} /></label><label>{t('pricing.currency')}<select value={currency} onChange={(event) => setCurrency(event.target.value)}><option value="USD">USD</option><option value="CNY">CNY</option></select></label>{schema ? <Form key={`${kind}-${locale}`} schema={localizeSchema(schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { const prefix = kind === 'generation' ? 'generation-prices' : 'prices'; await api(`/internal/v1/${prefix}/${encodeURIComponent(currency)}/${encodeURIComponent(model)}`, token, { method: 'POST', body: JSON.stringify(formData) }); setMessage(t('pricing.savedMessage')); setDisplayCurrency(currency); await load(currency); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant || !model.trim()}>{t('pricing.save')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}</div></details>
  </div>;
}

interface RouteDraft extends Pick<ModelRouteView, 'public_model' | 'upstream_model' | 'protocol' | 'priority'> {
  upstream_account_id: string;
  upstream_account_ids: string[];
  included_provider_group_ids: string[];
  excluded_provider_group_ids: string[];
  route_group_ids: string[];
  route_group_names: string[];
  granted_credential_ids: string[];
  custom_model_confirmed: boolean;
}
const emptyRouteDraft: RouteDraft = {
  public_model: '', upstream_account_id: '', upstream_account_ids: [], upstream_model: '', protocol: 'openai', priority: 0,
  included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: [], route_group_names: [], granted_credential_ids: [], custom_model_confirmed: false,
};

function selections(ids: string[], options: ComboboxOption[]) {
  return ids.map((id) => options.find((option) => option.value === id) ?? { value: id, label: id });
}

function routeRequest(draft: RouteDraft, customModelConfirmed: boolean) {
  const { upstream_account_id: _legacyAccountId, ...request } = draft;
  return { ...request, custom_model_confirmed: customModelConfirmed };
}

function RouteFields({ token, tenant, draft, upstreams, providers, providerGroups, routeGroups, credentials, onChange, onCatalogValidity }: {
  token: string;
  tenant: string;
  draft: RouteDraft;
  upstreams: UpstreamAccount[];
  providers: ProviderType[];
  providerGroups: GroupView[];
  routeGroups: GroupView[];
  credentials: KeyView[];
  onChange: (draft: RouteDraft) => void;
  onCatalogValidity: (valid: boolean, allowCustom: boolean) => void;
}) {
  const { locale, t } = useI18n();
  const knownProtocols = ['openai', 'anthropic', 'generation'];
  const includedAccountIds = providerGroups.filter((group) => draft.included_provider_group_ids.includes(group.id)).flatMap((group) => group.member_ids);
  const excludedAccountIds = new Set(providerGroups.filter((group) => draft.excluded_provider_group_ids.includes(group.id)).flatMap((group) => group.member_ids));
  const candidateIds = [...new Set([...draft.upstream_account_ids, ...includedAccountIds])].filter((id) => !excludedAccountIds.has(id));
  const candidateProtocolSets = candidateIds.map((id) => {
    const account = upstreams.find((value) => value.id === id);
    return providers.find((value) => value.id === account?.driver)?.protocols;
  });
  const supportedByAll = candidateIds.length === 0 || candidateProtocolSets.some((values) => !values)
    ? knownProtocols
    : knownProtocols.filter((protocol) => candidateProtocolSets.every((values) => values?.includes(protocol)));
  const protocolCompatible = supportedByAll.includes(draft.protocol);
  const upstreamOptions = upstreams.map((value) => ({ value: value.id, label: value.name, description: value.driver }));
  const providerGroupOptions = providerGroups.map((value) => ({ value: value.id, label: value.name, description: t('groups.memberCount', { count: formatNumber(value.member_count, locale) }) }));
  const routeGroupOptions = routeGroups.map((value) => ({ value: value.id, label: value.name, description: t('groups.memberCount', { count: formatNumber(value.member_count, locale) }) }));
  const credentialOptions = credentials.map((value) => ({ value: value.key_id, label: value.alias, description: value.key_id }));
  const routeGroupValue = [
    ...selections(draft.route_group_ids, routeGroupOptions),
    ...draft.route_group_names.map((name) => ({ value: `new:${name}`, label: name, created: true })),
  ];
  return <>
    <label>{t('routes.publicModel')}<input value={draft.public_model} onChange={(event) => onChange({ ...draft, public_model: event.target.value })} /></label>
    <MultiCombobox label={t('routes.explicitUpstreams')} options={upstreamOptions} value={selections(draft.upstream_account_ids, upstreamOptions)} onChange={(selected) => {
      const upstream_account_ids = selected.map((item) => item.value);
      const upstream_account_id = upstream_account_ids[0] ?? '';
      onChange({ ...draft, upstream_account_id, upstream_account_ids });
    }} placeholder={t('routes.searchUpstreams')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('routes.explicitUpstreamsHint')} />
    <div className="route-group-grid">
      <MultiCombobox label={t('routes.includeProviderGroups')} options={providerGroupOptions} value={selections(draft.included_provider_group_ids, providerGroupOptions)} onChange={(selected) => onChange({ ...draft, included_provider_group_ids: selected.map((item) => item.value) })} placeholder={t('routes.searchProviderGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
      <MultiCombobox label={t('routes.excludeProviderGroups')} options={providerGroupOptions} value={selections(draft.excluded_provider_group_ids, providerGroupOptions)} onChange={(selected) => onChange({ ...draft, excluded_provider_group_ids: selected.map((item) => item.value) })} placeholder={t('routes.searchProviderGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('routes.exclusionWins')} />
    </div>
    <label>{t('routes.protocol')}<select aria-invalid={!protocolCompatible} value={draft.protocol} onChange={(event) => onChange({ ...draft, protocol: event.target.value })}>{knownProtocols.map((protocol) => <option disabled={candidateIds.length > 0 && !supportedByAll.includes(protocol)} key={protocol} value={protocol}>{protocol === 'generation' ? t('routes.generation') : protocol === 'anthropic' ? 'Anthropic' : 'OpenAI'}</option>)}</select><small className={`field-hint${protocolCompatible ? '' : ' field-error'}`}>{t(protocolCompatible ? 'routes.protocolCompatibilityHint' : 'routes.protocolIncompatible')}</small></label>
    <UpstreamModelCombobox token={token} tenant={tenant} accountIds={draft.upstream_account_ids} includedProviderGroupIds={draft.included_provider_group_ids} excludedProviderGroupIds={draft.excluded_provider_group_ids} syncAccountIds={candidateIds} protocol={draft.protocol} value={draft.upstream_model} onChange={(upstream_model) => onChange({ ...draft, upstream_model, custom_model_confirmed: false })} customModelConfirmed={draft.custom_model_confirmed} onValidityChange={onCatalogValidity} />
    <label>{t('routes.priority')}<input type="number" min={-1000000} max={1000000} value={draft.priority} onChange={(event) => onChange({ ...draft, priority: Number(event.target.value) })} /></label>
    <MultiCombobox label={t('routes.routeGroups')} options={routeGroupOptions} value={routeGroupValue} onChange={(selected) => onChange({ ...draft, route_group_ids: selected.filter((item) => !item.created).map((item) => item.value), route_group_names: selected.filter((item) => item.created).map((item) => item.label) })} placeholder={t('routes.searchOrCreateRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} allowCreate createLabel={(name) => t('routes.createRouteGroupNamed', { name })} hint={t('routes.routeGroupsHint')} />
    <MultiCombobox label={t('routes.exactCredentials')} options={credentialOptions} value={selections(draft.granted_credential_ids, credentialOptions)} onChange={(selected) => onChange({ ...draft, granted_credential_ids: selected.map((item) => item.value) })} placeholder={t('routes.searchCredentials')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('routes.exactCredentialsHint')} />
  </>;
}

function RouteWorkspace({ token, tenant, upstreams, providers }: { token: string; tenant: string; upstreams: UpstreamAccount[]; providers: ProviderType[] }) {
  const { locale, t } = useI18n();
  const [routes, setRoutes] = useState<ModelRouteView[]>([]);
  const [credentials, setCredentials] = useState<KeyView[]>([]);
  const providerGroups = useGroups('provider', token, tenant);
  const routeGroups = useGroups('route', token, tenant);
  const [form, setForm] = useState<RouteDraft>(emptyRouteDraft);
  const [formCatalog, setFormCatalog] = useState({ valid: false, allowCustom: false });
  const [editing, setEditing] = useState<ModelRouteView>();
  const [editForm, setEditForm] = useState<RouteDraft>(emptyRouteDraft);
  const [editCatalog, setEditCatalog] = useState({ valid: false, allowCustom: false });
  const [busy, setBusy] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const load = async () => {
    if (!token || !tenant) { setRoutes([]); setCredentials([]); return; }
    try {
      const [nextRoutes, nextCredentials] = await Promise.all([
        api<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(tenant)}`, token),
        api<KeyView[]>(`/internal/v1/keys${queryForTenant(tenant)}`, token),
      ]);
      setRoutes(nextRoutes); setCredentials(nextCredentials); setError('');
    }
    catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => { setEditing(undefined); setMessage(''); void load(); }, [token, tenant]);
  const scopedUpstreams = upstreams.filter((value) => !value.tenant_external_id || value.tenant_external_id === tenant);
  const canSubmit = (draft: RouteDraft, catalogValid: boolean) => Boolean(tenant && catalogValid && draft.public_model.trim() && draft.upstream_model.trim()
    && (draft.upstream_account_ids.length > 0 || draft.included_provider_group_ids.length > 0));
  const beginEdit = (route: ModelRouteView) => {
    setEditing(route);
    setEditCatalog({ valid: false, allowCustom: false });
    setEditForm({
      public_model: route.public_model,
      upstream_account_id: route.upstream_account_id ?? route.upstream_account_ids?.[0] ?? '',
      upstream_account_ids: route.upstream_account_ids ?? (route.upstream_account_id ? [route.upstream_account_id] : []),
      upstream_model: route.upstream_model,
      protocol: route.protocol,
      priority: route.priority,
      included_provider_group_ids: route.included_provider_group_ids ?? [],
      excluded_provider_group_ids: route.excluded_provider_group_ids ?? [],
      route_group_ids: route.route_group_ids ?? [],
      route_group_names: [],
      granted_credential_ids: route.granted_credential_ids ?? [],
      custom_model_confirmed: route.custom_model_confirmed ?? false,
    });
    setMessage(''); setError('');
  };
  const saveEdit = async () => {
    if (!editing || !canSubmit(editForm, editCatalog.valid)) return;
    setBusy(editing.id); setMessage(''); setError('');
    try {
      await api(`/internal/v1/model-routes/${editing.id}`, token, { method: 'PUT', body: JSON.stringify({ ...routeRequest(editForm, editCatalog.allowCustom), tenant_external_id: tenant, expected_updated_at: editing.updated_at, expected_grant_revision: editing.grant_revision }) });
      setEditing(undefined); setMessage(t('routes.updated')); await Promise.all([load(), routeGroups.load]);
    } catch (reason) {
      if (reason instanceof ApiError && reason.status === 409) {
        setEditing(undefined); setError(t('routes.concurrentChangeReloaded')); await Promise.all([load(), routeGroups.load]);
      } else setError(messageOf(reason, t('common.requestFailed')));
    }
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
    <article className="panel"><div className="panel-title"><div><h2>{t('routes.title')}</h2><p className="muted">{t('routes.description')}</p></div><span>{formatNumber(routes.length, locale)}</span></div>{error && <div className="notice error" role="alert">{error}</div>}{providerGroups.error && <div className="notice error" role="alert">{providerGroups.error}</div>}{routeGroups.error && <div className="notice error" role="alert">{routeGroups.error}</div>}{message && <div className="notice success" role="status">{message}</div>}<div className="table-scroll"><table><thead><tr><th>{t('routes.publicModel')}</th><th>{t('routes.upstream')}</th><th>{t('routes.groups')}</th><th>{t('routes.upstreamModel')}</th><th>{t('routes.protocol')}</th><th>{t('routes.priority')}</th><th>{t('request.status')}</th><th>{t('routes.actions')}</th></tr></thead><tbody>{routes.map((route) => <tr key={route.id}><td><code>{route.public_model}</code></td><td><div className="table-chip-list">{(route.upstream_account_ids ?? (route.upstream_account_id ? [route.upstream_account_id] : [])).map((id) => <span key={id}>{scopedUpstreams.find((value) => value.id === id)?.name ?? id}</span>)}</div></td><td><div className="table-chip-list">{(route.route_group_ids ?? []).map((id) => <span key={id}>{routeGroups.groups.find((value) => value.id === id)?.name ?? id}</span>)}</div></td><td><code>{route.upstream_model}</code></td><td>{route.protocol}</td><td>{formatNumber(route.priority, locale)}</td><td><span className={`status ${route.enabled ? 'ok' : 'pending'}`}>{route.enabled ? t('common.enabled') : t('common.disabled')}</span></td><td><div className="row-actions"><button type="button" className="secondary" disabled={busy === route.id || !tenant} onClick={() => beginEdit(route)}>{t('routes.edit')}</button><button type="button" className="secondary" disabled={busy === route.id || !tenant} onClick={() => void setEnabled(route, !route.enabled)}>{route.enabled ? t('routes.disable') : t('routes.enable')}</button><button type="button" className="danger" title={route.enabled ? t('routes.disableBeforeDelete') : undefined} disabled={busy === route.id || !tenant || route.enabled} onClick={() => void remove(route)}>{t('common.remove')}</button></div></td></tr>)}</tbody></table>{routes.length === 0 && <div className="empty">{t('routes.empty')}</div>}</div>
      {editing && <div className="inline-editor form-panel"><div className="panel-title"><h3>{t('routes.editTitle', { model: editing.public_model })}</h3><button type="button" className="secondary" onClick={() => setEditing(undefined)}>{t('common.cancel')}</button></div><RouteFields token={token} tenant={tenant} draft={editForm} upstreams={scopedUpstreams} providers={providers} providerGroups={providerGroups.groups} routeGroups={routeGroups.groups} credentials={credentials} onChange={setEditForm} onCatalogValidity={(valid, allowCustom) => setEditCatalog({ valid, allowCustom })} /><button type="button" disabled={busy === editing.id || !canSubmit(editForm, editCatalog.valid)} onClick={() => void saveEdit()}>{t('common.save')}</button></div>}
    </article>
    <article className="panel form-panel"><h2>{t('routes.createTitle')}</h2><RouteFields token={token} tenant={tenant} draft={form} upstreams={scopedUpstreams} providers={providers} providerGroups={providerGroups.groups} routeGroups={routeGroups.groups} credentials={credentials} onChange={setForm} onCatalogValidity={(valid, allowCustom) => setFormCatalog({ valid, allowCustom })} /><button type="button" disabled={busy === 'create' || !canSubmit(form, formCatalog.valid)} onClick={async () => { setBusy('create'); setMessage(''); setError(''); try { await api('/internal/v1/model-routes', token, { method: 'POST', body: JSON.stringify({ ...routeRequest(form, formCatalog.allowCustom), tenant_external_id: tenant }) }); setForm(emptyRouteDraft); setFormCatalog({ valid: false, allowCustom: false }); setMessage(t('routes.created')); await Promise.all([load(), routeGroups.load]); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}>{t('routes.create')}</button></article>
  </section><section className="routing-group-managers">
    <GroupManager kind="provider" token={token} tenant={tenant} groups={providerGroups.groups} resources={scopedUpstreams.map((value) => ({ value: value.id, label: value.name, description: value.driver }))} onChanged={providerGroups.load} />
    <GroupManager kind="route" token={token} tenant={tenant} groups={routeGroups.groups} resources={routes.map((route) => ({ value: route.id, label: route.public_model, description: route.protocol }))} onChanged={async () => { await Promise.all([routeGroups.load(), load()]); }} />
  </section></>;
}

function CredentialWorkspace({ token, tenant, createSchema, policySchema }: { token: string; tenant: string; createSchema?: Record<string, unknown>; policySchema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const [values, setValues] = useState<KeyView[]>([]);
  const [routes, setRoutes] = useState<ModelRouteView[]>([]);
  const [editingPolicy, setEditingPolicy] = useState<string>();
  const [editingRouting, setEditingRouting] = useState<string>();
  const [routingDraft, setRoutingDraft] = useState<CredentialRoutingView>();
  const [renaming, setRenaming] = useState<string>();
  const [aliasDraft, setAliasDraft] = useState('');
  const [limitSnapshots, setLimitSnapshots] = useState<Record<string, KeyLimitSnapshot>>({});
  const [granting, setGranting] = useState<string>();
  const [grant, setGrant] = useState({ amount: '', source: 'operator-console' });
  const [newRouteIds, setNewRouteIds] = useState<string[]>([]);
  const [newRouteGroupIds, setNewRouteGroupIds] = useState<string[]>([]);
  const [groupFilter, setGroupFilter] = useState('all');
  const [secret, setSecret] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const credentialGroups = useGroups('credential', token, tenant);
  const routeGroups = useGroups('route', token, tenant);
  const createFormSchema = createSchema;
  const policyFormSchema = policySchema;
  const load = async () => {
    if (!token || !tenant) { setValues([]); setRoutes([]); return; }
    try {
      const [nextValues, nextRoutes] = await Promise.all([
        api<KeyView[]>(`/internal/v1/keys${queryForTenant(tenant)}`, token),
        api<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(tenant)}`, token),
      ]);
      setValues(nextValues); setRoutes(nextRoutes); setError('');
    }
    catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => { setRenaming(undefined); setEditingRouting(undefined); setRoutingDraft(undefined); setLimitSnapshots({}); setGroupFilter('all'); void load(); }, [token, tenant]);
  const filteredValues = values.filter((value) => {
    if (groupFilter === 'all') return true;
    const memberships = credentialGroups.groups.filter((group) => group.member_ids.includes(value.key_id));
    return groupFilter === 'unassigned' ? memberships.length === 0 : memberships.some((group) => group.id === groupFilter);
  });
  const routeOptions = routes.map((route) => ({ value: route.id, label: route.public_model, description: route.protocol }));
  const routeGroupOptions = routeGroups.groups.map((group) => ({ value: group.id, label: group.name, description: t('groups.memberCount', { count: formatNumber(group.member_count, locale) }) }));
  const openRouting = async (value: KeyView) => {
    if (editingRouting === value.key_id) { setEditingRouting(undefined); setRoutingDraft(undefined); return; }
    setError('');
    try {
      const routing = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing${queryForTenant(tenant)}`, token);
      setRoutingDraft(routing); setEditingRouting(value.key_id);
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
  };
  const saveRouting = async (value: KeyView, draft: CredentialRoutingView) => {
    try {
      const saved = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing`, token, { method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, route_ids: draft.route_ids, route_group_ids: draft.route_group_ids, expected_grant_revision: draft.grant_revision }) });
      setRoutingDraft(saved); setMessage(t('credentials.routingSaved')); setError('');
    } catch (reason) {
      if (reason instanceof ApiError && reason.status === 409) {
        const current = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing${queryForTenant(tenant)}`, token);
        setRoutingDraft(current); setError(t('credentials.concurrentRoutingReloaded'));
      } else setError(messageOf(reason, t('common.requestFailed')));
    }
  };
  return <><WriteScopeNotice tenant={tenant} />{secret && <OneTimeSecret value={secret} message={t('credentials.oneTimeSecret')} />}<section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('credentials.title')}</h2><p className="muted">{t('credentials.description')}</p></div><span>{formatNumber(filteredValues.length, locale)}</span></div>
      <label className="credential-group-filter">{t('credentials.groupFilter')}<select value={groupFilter} onChange={(event) => setGroupFilter(event.target.value)}><option value="all">{t('common.all')}</option><option value="unassigned">{t('credentials.ungrouped')}</option>{credentialGroups.groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}</select></label>
      {error && <div className="notice error" role="alert">{error}</div>}{credentialGroups.error && <div className="notice error" role="alert">{credentialGroups.error}</div>}{routeGroups.error && <div className="notice error" role="alert">{routeGroups.error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      <div className="account-list">{filteredValues.length === 0 && <div className="empty">{values.length === 0 ? t('credentials.empty') : t('credentials.noGroupResults')}</div>}{filteredValues.map((value) => {
        const memberships = credentialGroups.groups.filter((group) => group.member_ids.includes(value.key_id));
        return <div className="managed-resource" key={value.key_id}><div className="managed-resource-header"><div><b>{value.alias}</b><small>{value.key_id}</small><span>{value.principal_external_id ?? t('common.unknownPrincipal')} · {formatCurrency(value.available_balance, value.currency, locale)}</span></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : value.status === 'revoked' ? 'bad' : 'pending'}`}>{enumLabel(t, 'status', value.status ?? 'active')}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span></div></div>
          {memberships.length > 0 && <div className="table-chip-list credential-group-chips" aria-label={t('groups.credential.title')}>{memberships.map((group) => <span key={group.id}>{group.name}</span>)}</div>}
          <div className="policy-chips"><span>RPM {formatNumber(value.policy.requests_per_minute, locale)}</span><span>TPM {formatNumber(value.policy.tokens_per_minute, locale)}</span><span>{t('self.concurrency')} {formatNumber(value.policy.max_concurrency, locale)}</span><span>{t('budget.daily')}: {value.policy.daily_budget === null ? '—' : formatCurrency(value.policy.daily_budget, value.currency, locale)}</span><span>{t('budget.weekly')}: {value.policy.weekly_budget === null ? '—' : formatCurrency(value.policy.weekly_budget, value.currency, locale)}</span><span>{t('budget.lifetime')}: {value.policy.lifetime_budget === null ? '—' : formatCurrency(value.policy.lifetime_budget, value.currency, locale)}</span></div>
          <div className="row-actions"><button type="button" className="secondary" disabled={!tenant} onClick={() => { setRenaming(renaming === value.key_id ? undefined : value.key_id); setAliasDraft(value.alias); }}>{t('credentials.rename')}</button><button type="button" className="secondary" disabled={!tenant} onClick={async () => { try { const snapshot = await api<KeyLimitSnapshot>(`/internal/v1/keys/${value.key_id}/limits`, token); setLimitSnapshots((current) => ({ ...current, [value.key_id]: snapshot })); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('credentials.viewLimits')}</button><button type="button" className="secondary" disabled={!tenant || value.status === 'revoked'} onClick={async () => { try { const result = await api<{ key: string }>(`/internal/v1/keys/${value.key_id}/rotate`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } }); setSecret(result.key); setMessage(t('credentials.rotated', { alias: value.alias })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('credentials.rotate')}</button><button type="button" className="secondary" disabled={!tenant || value.status === 'revoked'} onClick={() => setEditingPolicy(editingPolicy === value.key_id ? undefined : value.key_id)}>{t('credentials.editPolicy')}</button><button type="button" className="secondary" disabled={!tenant || value.status === 'revoked'} onClick={() => void openRouting(value)}>{t('credentials.routing')}</button><button type="button" className="secondary" disabled={!tenant || !value.account_id || value.status === 'revoked'} title={!value.account_id ? t('credentials.accountMissing') : undefined} onClick={() => setGranting(granting === value.key_id ? undefined : value.key_id)}>{t('credentials.grant')}</button>{value.status !== 'revoked' && <button type="button" className="secondary" disabled={!tenant} onClick={async () => { const nextStatus = value.status === 'active' ? 'suspended' : 'active'; try { await api(`/internal/v1/keys/${value.key_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); setMessage(t(nextStatus === 'active' ? 'credentials.resumed' : 'credentials.suspended', { alias: value.alias })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{value.status === 'active' ? t('credentials.suspend') : t('credentials.resume')}</button>}</div>
          {renaming === value.key_id && <div className="inline-editor form-panel"><h3>{t('credentials.renameFor', { alias: value.alias })}</h3><label>{t('schema.Credential alias')}<input value={aliasDraft} maxLength={200} onChange={(event) => setAliasDraft(event.target.value)} /></label><button type="button" disabled={!aliasDraft.trim()} onClick={async () => { try { await api(`/internal/v1/keys/${value.key_id}/alias`, token, { method: 'PATCH', body: JSON.stringify({ alias: aliasDraft }) }); setRenaming(undefined); setMessage(t('credentials.renamed', { alias: aliasDraft.trim() })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('common.save')}</button></div>}
          {limitSnapshots[value.key_id] && <LimitSnapshot value={limitSnapshots[value.key_id]} />}
          {editingPolicy === value.key_id && policyFormSchema && <div className="inline-editor form-panel"><h3>{t('credentials.policyFor', { alias: value.alias })}</h3><Form key={`${value.key_id}-${locale}`} schema={localizeSchema(policyFormSchema as RJSFSchema, locale)} formData={value.policy} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { try { await api(`/internal/v1/keys/${value.key_id}/policy`, token, { method: 'PUT', body: JSON.stringify(formData) }); setEditingPolicy(undefined); setMessage(t('credentials.policySaved')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant}>{t('common.save')}</button></Form></div>}
          {editingRouting === value.key_id && routingDraft && <div className="inline-editor form-panel routing-editor"><h3>{t('credentials.routingFor', { alias: value.alias })}</h3><p className="muted">{t('credentials.routingHint')}</p>
            <MultiCombobox label={t('credentials.exactRoutes')} options={routeOptions} value={selections(routingDraft.route_ids, routeOptions)} onChange={(selected) => setRoutingDraft({ ...routingDraft, route_ids: selected.map((item) => item.value) })} placeholder={t('credentials.searchRoutes')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
            <MultiCombobox label={t('credentials.routeGroups')} options={routeGroupOptions} value={selections(routingDraft.route_group_ids, routeGroupOptions)} onChange={(selected) => setRoutingDraft({ ...routingDraft, route_group_ids: selected.map((item) => item.value) })} placeholder={t('credentials.searchRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('credentials.existingGroupsOnly')} />
            {routingDraft.effective_route_ids.length > 0 && <small className="field-hint">{t('credentials.effectiveRoutes', { count: formatNumber(routingDraft.effective_route_ids.length, locale) })}</small>}
            <button type="button" onClick={() => void saveRouting(value, routingDraft)}>{t('common.save')}</button>
          </div>}
          {granting === value.key_id && value.account_id && <div className="inline-editor form-panel"><h3>{t('credentials.grantFor', { alias: value.alias })}</h3><label>{t('credentials.grantAmount')}<input inputMode="decimal" value={grant.amount} onChange={(event) => setGrant({ ...grant, amount: event.target.value })} /></label><label>{t('credentials.grantSource')}<input value={grant.source} onChange={(event) => setGrant({ ...grant, source: event.target.value })} /></label><button type="button" disabled={!grant.amount || !grant.source.trim()} onClick={async () => { try { await api(`/internal/v1/accounts/${value.account_id}/grants`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify(grant) }); setGranting(undefined); setGrant({ amount: '', source: 'operator-console' }); setMessage(t('credentials.granted')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('credentials.confirmGrant')}</button></div>}
        </div>})}</div></article>
    <article className="panel form-panel"><h2>{t('credentials.createTitle')}</h2><p className="muted">{t('credentials.createRoutingHint')}</p>
      <MultiCombobox label={t('credentials.exactRoutes')} options={routeOptions} value={selections(newRouteIds, routeOptions)} onChange={(selected) => setNewRouteIds(selected.map((item) => item.value))} placeholder={t('credentials.searchRoutes')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
      <MultiCombobox label={t('credentials.routeGroups')} options={routeGroupOptions} value={selections(newRouteGroupIds, routeGroupOptions)} onChange={(selected) => setNewRouteGroupIds(selected.map((item) => item.value))} placeholder={t('credentials.searchRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('credentials.existingGroupsOnly')} />
      {createFormSchema ? <Form key={`${tenant}-${locale}`} schema={localizeSchema(createFormSchema as RJSFSchema, locale)} uiSchema={{ tenant_external_id: { 'ui:widget': 'hidden' } }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try {
        const created = await api<{ key: string; key_id: string }>('/internal/v1/keys', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant, route_ids: newRouteIds, route_group_ids: newRouteGroupIds }) });
        setNewRouteIds([]); setNewRouteGroupIds([]); setSecret(created.key); setMessage(t(newRouteIds.length || newRouteGroupIds.length ? 'credentials.created' : 'credentials.createdNoRoutes')); await load();
      } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant}>{t('credentials.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
    </article>
  </section><GroupManager kind="credential" token={token} tenant={tenant} groups={credentialGroups.groups} resources={values.map((value) => ({ value: value.key_id, label: value.alias, description: value.key_id }))} onChanged={credentialGroups.load} /></>;
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

function RequestDrawer({ detail, onClose }: { detail: RequestDetail; onClose: () => void }) {
  const { t } = useI18n();
  return <DrawerFrame title={detail.model} eyebrow={t('request.operatorDiagnosis')} onClose={onClose}><p className="muted break-anywhere">{detail.request_id} · {detail.status_code ?? t('common.running')} · {detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</p><h3>{t('request.error')}</h3><pre>{detail.error_code ?? t('common.none')}</pre><h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre><h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre></DrawerFrame>;
}
