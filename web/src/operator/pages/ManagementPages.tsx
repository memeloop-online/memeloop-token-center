import RjsfForm, { type FormProps } from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ApiError, api, apiRead } from '../../api';
import { formatCurrency, formatNumber } from '../../format';
import { localizeSchema, useI18n } from '../../i18n';
import { LimitSnapshot } from '../../LimitSnapshot';
import { schemaFormFields, schemaFormTemplates } from '../../SchemaTemplates';
import { safeValidator as validator } from '../../safeValidator';
import type {
  ConfigurationSchemas, CredentialRoutingView, GenerationPriceView, GroupView, KeyLimitSnapshot, KeyView,
  ModelPriceSyncResult, ModelPriceUsageSummary, ModelPriceView, ModelRouteView, ProviderType,
  ServiceTokenView, UpstreamAccount, UpstreamHealth,
} from '../../types';
import { GroupManager, useGroups } from '../GroupManager';
import { MultiCombobox, type ComboboxOption } from '../MultiCombobox';
import { UpstreamModelCombobox } from '../UpstreamModelCombobox';
import { directCredentialSchema, supportsDirectConnection } from '../providerConnectionMethods';
import { useOperatorResource, type ResourceState } from '../hooks/useOperatorResource';
import { enumLabel, messageOf, OneTimeSecret, queryForTenant, WriteScopeNotice } from '../scope/operatorShared';

function Form(props: FormProps) {
  return <RjsfForm {...props} noHtml5Validate onError={() => { /* Validation is rendered inline. */ }} />;
}

function isPositiveDecimal(value: string) {
  const normalized = value.trim();
  return /^(?:\d+(?:\.\d+)?|\.\d+)$/.test(normalized) && /[1-9]/.test(normalized);
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
  const [showDisabled, setShowDisabled] = useState(false);
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
  useEffect(() => {
    setMethod('direct'); setDriver(''); setRotating(undefined); setEditing(undefined); setReauthorizing(undefined);
    setBusy(''); setHealth({}); setMessage(''); setError(''); setShowDisabled(false);
  }, [token, tenant]);

  const activeValues = values.filter((value) => value.status === 'active');
  const disabledValues = values.filter((value) => value.status !== 'active');
  const visibleValues = showDisabled ? [...activeValues, ...disabledValues] : activeValues;

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
      if (status === 'disabled') setShowDisabled(true);
      setHealth((current) => { const next = { ...current }; delete next[value.id]; return next; });
      setMessage(t(status === 'active' ? 'providers.enabled' : 'providers.disabled', { name: value.name }));
      await onChanged();
      setHealth((current) => { const next = { ...current }; delete next[value.id]; return next; });
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
    <article className="panel provider-list"><div className="panel-title"><div><h2>{t('providers.title')}</h2><p className="muted">{t('providers.description')}</p></div><div className="provider-list-actions"><span>{t('providers.activeCount', { active: formatNumber(activeValues.length, locale), total: formatNumber(values.length, locale) })}</span>{disabledValues.length > 0 && <button type="button" className="secondary" aria-expanded={showDisabled} onClick={() => setShowDisabled((current) => !current)}>{showDisabled ? t('providers.hideDisabled') : t('providers.showDisabled', { count: formatNumber(disabledValues.length, locale) })}</button>}</div></div>
      {error && <div className="notice error" role="alert">{error}</div>}{providerGroups.error && <div className="notice error" role="alert">{providerGroups.error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      <div className="account-list">{visibleValues.length === 0 && <div className="empty">{values.length === 0 ? t('providers.empty') : t('providers.noActive')}</div>}{visibleValues.map((value) => {
        const currentHealth = value.status === 'active' ? health[value.id] : undefined;
        const manageable = canManage(value);
        const memberships = providerGroups.groups.filter((group) => group.member_ids.includes(value.id));
        return <div className="account provider-account" key={value.id}><div className="account-main"><b>{value.name}</b><span>{value.driver} · {t('providers.method')}: {enumLabel(t, 'auth', value.connection_method)}{value.tenant_external_id ? ` · ${value.tenant_external_id}` : ''}</span>{memberships.length > 0 && <div className="table-chip-list provider-group-summary" aria-label={t('groups.provider.title')}>{memberships.map((group) => <span key={group.id}>{group.name}</span>)}</div>}<small>{value.id}</small>{value.credential_expires_at && <small>{t('providers.expires')}: {new Date(value.credential_expires_at).toLocaleString(locale)}</small>}{currentHealth && <small className={`status ${currentHealth.status === 'healthy' ? 'ok' : 'pending'}`}>{currentHealth.status === 'healthy' ? t('providers.healthy') : t('providers.unhealthy')}{currentHealth.upstream_status ? ` · HTTP ${formatNumber(currentHealth.upstream_status, locale)}` : ''}{currentHealth.latency_ms !== undefined ? ` · ${formatNumber(currentHealth.latency_ms, locale, 2)} ms` : ''}</small>}</div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : 'pending'}`}>{enumLabel(t, 'status', value.status)}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span><span className="pill">{t('providers.routes', { count: formatNumber(value.route_count, locale) })}</span><div className="row-actions"><button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setEditing(value)}>{t('providers.edit')}</button><button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void checkHealth(value)}>{t('providers.health')}</button>{value.can_refresh && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void refreshOAuth(value)}>{t('providers.refreshAuthorization')}</button>}{value.can_reauthorize && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setReauthorizing(value)}>{t('providers.reauthorize')}</button>}{value.auth_kind === 'oauth' && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void disconnectOAuth(value)}>{t('providers.disconnect')}</button>}{value.can_rotate && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setRotating(value)}>{t('providers.rotateCredential')}</button>}<button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void setStatus(value, value.status === 'active' ? 'disabled' : 'active')}>{value.status === 'active' ? t('providers.disable') : t('providers.enable')}</button><button type="button" className="danger" title={value.status !== 'disabled' ? t('providers.disableBeforeDelete') : value.route_count > 0 ? t('providers.removeRoutesFirst') : undefined} disabled={!manageable || Boolean(busy) || value.status !== 'disabled' || value.route_count > 0} onClick={() => void remove(value)}>{t('common.remove')}</button></div></div></div>;
      })}</div>
      {editing && editSchema && <div className="inline-editor"><div className="panel-title"><h3>{t('providers.editFor', { name: editing.name })}</h3><button type="button" className="secondary" onClick={() => setEditing(undefined)}>{t('common.cancel')}</button></div><Form key={`${editing.id}-${locale}`} schema={editSchema} uiSchema={{ config: { oauth: { 'ui:disabled': true } } }} formData={{ name: editing.name, config: editing.config }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!formData) return; setBusy(`edit-${editing.id}`); try { await api(`/internal/v1/upstreams/${editing.id}`, token, { method: 'PUT', body: JSON.stringify({ ...formData, tenant_external_id: tenant, expected_updated_at: editing.updated_at }) }); setEditing(undefined); setMessage(t('providers.updated', { name: editing.name })); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}><button type="submit" disabled={!canManage(editing) || Boolean(busy)}>{t('common.save')}</button></Form></div>}
      {rotating && rotateProvider && <div className="inline-editor"><div className="panel-title"><h3>{t('providers.rotateFor', { name: rotating.name })}</h3><button type="button" className="secondary" onClick={() => setRotating(undefined)}>{t('common.cancel')}</button></div><Form key={`${rotating.id}-${locale}`} schema={localizeSchema(rotateProvider.credential_schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { setBusy(`rotate-${rotating.id}`); try { await api(`/internal/v1/upstreams/${rotating.id}/credential`, token, { method: 'PUT', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify({ credential: formData }) }); setRotating(undefined); setMessage(t('providers.rotated', { name: rotating.name })); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}><button type="submit" disabled={!canManage(rotating) || Boolean(busy)}>{t('providers.confirmRotate')}</button></Form></div>}
    </article>
    <details key={reauthorizing?.id ?? 'provider-create'} className="panel create-resource provider-onboarding" open={reauthorizing ? true : undefined}><summary><span><b>{reauthorizing ? t('providers.reauthorizeFor', { name: reauthorizing.name }) : t('providers.add')}</b><small>{t('providers.description')}</small></span><span aria-hidden="true">＋</span></summary><div className="create-resource-body form-panel">{reauthorizing ? <>
      <div className="panel-title"><h2>{t('providers.reauthorizeFor', { name: reauthorizing.name })}</h2><button type="button" className="secondary" onClick={() => setReauthorizing(undefined)}>{t('common.cancel')}</button></div>
      <AuthorizationConnection key={`reauthorize-${reauthorizing.id}`} token={token} tenant={tenant} providers={providers} existing={reauthorizing} onChanged={async () => { setReauthorizing(undefined); setMessage(t('providers.reauthorized', { name: reauthorizing.name })); await onChanged(); }} />
    </> : <>
      <div className="segmented" role="group" aria-label={t('providers.method')}><button type="button" aria-pressed={method === 'direct'} className={method === 'direct' ? 'active' : ''} onClick={() => setMethod('direct')}>{t('providers.direct')}</button><button type="button" aria-pressed={method === 'authorization'} className={method === 'authorization' ? 'active' : ''} onClick={() => setMethod('authorization')}>{t('providers.oauth')}</button></div>
      {method === 'direct' ? <>
        <label>{t('providers.provider')}<select value={provider?.id ?? ''} onChange={(event) => setDriver(event.target.value)}>{directProviders.map((value) => <option key={value.id} value={value.id}>{value.display_name} · {value.source}</option>)}</select></label>
        {schema ? <Form key={`${provider.id}-${locale}`} schema={schema} uiSchema={uiSchema} fields={schemaFormFields} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { setError(''); await api('/internal/v1/upstreams', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant }) }); setMessage(t('providers.created')); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant || !token}>{t('providers.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
      </> : <AuthorizationConnection token={token} tenant={tenant} providers={providers} onChanged={onChanged} />}</>}</div>
    </details>
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
  const [loadedCurrency, setLoadedCurrency] = useState('');
  const [pricingLoading, setPricingLoading] = useState(false);
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const syncSequence = useRef(0);
  const scopeRef = useRef({ token, tenant, displayCurrency });
  scopeRef.current = { token, tenant, displayCurrency };
  const load = async (requestedCurrency = displayCurrency) => {
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken) return;
    setPricingLoading(true); setPrices([]); setGenerationPrices([]);
    const scope = queryForTenant(loadTenant);
    const results = await Promise.allSettled([
      api<ModelPriceView[]>(`/internal/v1/model-prices?currency=${encodeURIComponent(requestedCurrency)}`, loadToken),
      api<ModelPriceUsageSummary>(`/internal/v1/model-prices/usage-summary${scope}`, loadToken),
      api<GenerationPriceView[]>(`/internal/v1/generation-prices?currency=${encodeURIComponent(requestedCurrency)}`, loadToken),
    ]);
    if (sequence !== loadSequence.current || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
    const [nextPrices, nextUsage, nextGenerationPrices] = results;
    setPrices(nextPrices.status === 'fulfilled' ? nextPrices.value : []);
    setUsage(nextUsage.status === 'fulfilled' ? nextUsage.value : { models: [] });
    setGenerationPrices(nextGenerationPrices.status === 'fulfilled' ? nextGenerationPrices.value : []);
    setLoadedCurrency(requestedCurrency); setPricingLoading(false);
    const failures = results.filter((result) => result.status === 'rejected');
    setError(failures.length ? t('pricing.partialLoad', { count: formatNumber(failures.length, locale) }) : '');
  };
  useEffect(() => {
    loadSequence.current += 1;
    syncSequence.current += 1;
    setPrices([]); setGenerationPrices([]); setUsage({ models: [] }); setSyncResult(undefined); setLoadedCurrency('');
    setPricingLoading(false); setSyncing(false); setError(''); setMessage(''); setKind('token'); setModel('');
  }, [token, tenant]);
  useEffect(() => { void load(displayCurrency); }, [token, tenant, displayCurrency]);
  const usageByModel = new Map(usage.models.map((value) => [value.model, value]));
  const renderCurrency = loadedCurrency || displayCurrency;
  const rows = Array.from(new Set([...usage.models.map((value) => value.model), ...prices.map((value) => value.model)])).sort().flatMap((name) => {
    const price = prices.find((value) => value.model === name);
    const tiers = price?.tiers?.length ? price.tiers : price ? [{ service_tier: 'default', input_per_million: price.input_per_million, cached_input_per_million: price.input_per_million, cache_write_per_million: price.input_per_million, output_per_million: price.output_per_million, source: price.source, updated_at: price.updated_at, cache_price_estimated: true }] : [undefined];
    return tiers.map((tier, index) => ({ model: name, usage: index === 0 ? usageByModel.get(name) : undefined, tier }));
  });
  const schema = kind === 'generation' ? schemas?.generation_price : schemas?.model_price;
  const sync = async () => {
    if (!tenant) return;
    const syncToken = token; const syncTenant = tenant; const syncCurrency = displayCurrency;
    const sequence = ++syncSequence.current;
    setSyncing(true); setError(''); setMessage('');
    try {
      const result = await api<ModelPriceSyncResult>('/internal/v1/model-prices/sync', syncToken, { method: 'POST', body: JSON.stringify({ models: usage.models.map((value) => value.model), currency: displayCurrency, tenant_external_id: syncTenant }) });
      if (sequence !== syncSequence.current || scopeRef.current.token !== syncToken || scopeRef.current.tenant !== syncTenant || scopeRef.current.displayCurrency !== syncCurrency) return;
      setSyncResult(result); setPrices(result.prices); setLoadedCurrency(syncCurrency); setMessage(t('pricing.synced', { count: formatNumber(result.imported, locale) }));
    } catch (reason) { if (sequence === syncSequence.current && scopeRef.current.token === syncToken && scopeRef.current.tenant === syncTenant && scopeRef.current.displayCurrency === syncCurrency) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { if (sequence === syncSequence.current && scopeRef.current.token === syncToken && scopeRef.current.tenant === syncTenant && scopeRef.current.displayCurrency === syncCurrency) setSyncing(false); }
  };
  return <div className="pricing-page"><WriteScopeNotice tenant={tenant} />
    <article className="panel pricing-overview"><div className="panel-title"><div><h2>{t('pricing.title')}</h2><p className="muted">{t('pricing.description')}</p></div><div className="pricing-heading-actions"><label>{t('pricing.viewCurrency')}<select aria-label={t('pricing.viewCurrency')} value={displayCurrency} onChange={(event) => { const next = event.target.value; syncSequence.current += 1; setSyncing(false); setSyncResult(undefined); setMessage(''); setDisplayCurrency(next); setCurrency(next); }}><option value="USD">USD</option><option value="CNY">CNY</option></select></label><div className="disabled-action"><button type="button" onClick={() => void sync()} disabled={!tenant || syncing} title={!tenant ? t('pricing.syncNeedsTenant') : undefined}>{syncing ? t('pricing.syncing') : t('pricing.sync')}</button>{!tenant && <small>{t('pricing.syncNeedsTenant')}</small>}</div></div></div>
      <div className="pricing-summary"><span>{t('pricing.usedModels', { count: formatNumber(usage.models.length, locale) })}</span><span>{t('pricing.saved', { count: formatNumber(prices.length, locale) })}</span><span>{t('pricing.sourceOrder')}: models.dev → LiteLLM → OpenRouter</span></div>
      {error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      {syncResult && <><div className="source-status">{syncResult.sourceResults.map((source) => <div className={`source-card ${source.error ? 'failed' : 'healthy'}`} key={source.source}><b>{source.source}</b><span>{source.error ? t('pricing.sourceFailed') : t('pricing.sourceHealthy', { count: formatNumber(source.models, locale) })}</span>{source.error && <small>{source.error}</small>}</div>)}</div><div className="notice success"><b>{t('pricing.result')}</b> · {t('pricing.imported', { count: formatNumber(syncResult.imported, locale) })} · {t('pricing.candidates', { count: formatNumber(syncResult.candidates.length, locale) })} · {t('pricing.unmatched', { count: formatNumber(syncResult.unmatched.length, locale) })} · {t('pricing.preserved', { count: formatNumber(syncResult.preserved.length, locale) })}</div>
        {(syncResult.candidates.length > 0 || syncResult.unmatched.length > 0) && <div className="sync-details"><h3>{t('pricing.candidateDetails')}</h3>{syncResult.candidates.map((candidate) => <details key={candidate.model}><summary><code>{candidate.model}</code><span>{t('pricing.candidateCount', { count: formatNumber(candidate.candidates.length, locale) })}</span></summary><div className="candidate-list">{candidate.candidates.map((match) => <div key={`${match.source}-${match.sourceModelId}-${match.serviceTier}`}><b>{match.sourceModelId}</b><span>{match.source} · {match.serviceTier} · {match.reason}</span><code>{t('pricing.input')}: {formatCurrency(match.inputPerMillion, renderCurrency, locale)} · {t('pricing.output')}: {formatCurrency(match.outputPerMillion, renderCurrency, locale)}</code></div>)}</div></details>)}{syncResult.unmatched.length > 0 && <details><summary>{t('pricing.unmatchedModels')}</summary><div className="model-name-list">{syncResult.unmatched.map((name) => <code key={name}>{name}</code>)}</div></details>}</div>}
      </>}
      <div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.calls')}</th><th>{t('pricing.serviceTier')}</th><th>{t('pricing.input')}</th><th>{t('pricing.cachedInput')}</th><th>{t('pricing.cacheWrite')}</th><th>{t('pricing.output')}</th><th>{t('pricing.source')}</th><th>{t('pricing.updated')}</th></tr></thead><tbody>{rows.map((row) => <tr key={`${row.model}-${row.tier?.service_tier ?? 'missing'}`}><td><code>{row.model}</code></td><td>{row.usage ? formatNumber(row.usage.calls, locale) : ''}</td><td>{row.tier?.service_tier ?? '—'}</td><td>{row.tier ? formatCurrency(row.tier.input_per_million, renderCurrency, locale) : '—'}</td><td>{row.tier ? <>{formatCurrency(row.tier.cached_input_per_million, renderCurrency, locale)}{row.tier.cache_price_estimated && <small className="muted"> {t('pricing.estimated')}</small>}</> : '—'}</td><td>{row.tier ? <>{formatCurrency(row.tier.cache_write_per_million, renderCurrency, locale)}{row.tier.cache_price_estimated && <small className="muted"> {t('pricing.estimated')}</small>}</> : '—'}</td><td>{row.tier ? formatCurrency(row.tier.output_per_million, renderCurrency, locale) : '—'}</td><td>{row.tier ? <span className={`pill source-${row.tier.source.replace('.', '-')}`}>{row.tier.source}</span> : <span className="status pending">{t('pricing.missing')}</span>}</td><td>{row.tier ? new Date(row.tier.updated_at).toLocaleString(locale) : '—'}</td></tr>)}</tbody></table>{rows.length === 0 && <div className="empty">{pricingLoading ? t('common.loading') : t('pricing.noPricesForCurrency', { currency: renderCurrency })}</div>}</div>
    </article>
    <article className="panel"><div className="panel-title"><h2>{t('pricing.generationPrices')}</h2><span>{formatNumber(generationPrices.length, locale)}</span></div><div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.currency')}</th><th>{t('self.units')}</th><th>{t('pricing.unitPrice')}</th></tr></thead><tbody>{generationPrices.map((price) => <tr key={`${price.currency}-${price.model}`}><td><code>{price.model}</code></td><td>{price.currency}</td><td>{enumLabel(t, 'billingUnit', price.billing_unit)}</td><td>{formatCurrency(price.price_per_unit, price.currency, locale)}</td></tr>)}</tbody></table>{generationPrices.length === 0 && <div className="empty">{t('pricing.noGenerationPrices')}</div>}</div></article>
    <details className="panel manual-pricing"><summary><span><b>{t('pricing.manual')}</b><small>{t('pricing.manualHint')}</small></span><span>＋</span></summary><div className="manual-pricing-body form-panel"><label>{t('pricing.type')}<select value={kind} onChange={(event) => setKind(event.target.value as typeof kind)}><option value="token">{t('pricing.tokenModel')}</option><option value="generation">{t('pricing.generationModel')}</option></select></label><label>{t('pricing.model')}<input value={model} onChange={(event) => setModel(event.target.value)} /></label><label>{t('pricing.currency')}<select value={currency} onChange={(event) => setCurrency(event.target.value)}><option value="USD">USD</option><option value="CNY">CNY</option></select></label>{schema ? <Form key={`${kind}-${locale}`} schema={localizeSchema(schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { const prefix = kind === 'generation' ? 'generation-prices' : 'prices'; await api(`/internal/v1/${prefix}/${encodeURIComponent(currency)}/${encodeURIComponent(model)}`, token, { method: 'POST', body: JSON.stringify(formData) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setMessage(t('pricing.savedMessage')); if (currency === displayCurrency) await load(currency); else setDisplayCurrency(currency); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant || !model.trim()}>{t('pricing.save')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}</div></details>
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

function RouteFields({ token, tenant, draft, upstreams, providers, providerGroups, routeGroups, credentials, onChange, onCatalogValidity, onCredentialQuery }: {
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
  onCredentialQuery: (query: string) => void;
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
    <MultiCombobox label={t('routes.exactCredentials')} options={credentialOptions} value={selections(draft.granted_credential_ids, credentialOptions)} onChange={(selected) => onChange({ ...draft, granted_credential_ids: selected.map((item) => item.value) })} placeholder={t('routes.searchCredentials')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('routes.exactCredentialsHint')} onQueryChange={onCredentialQuery} />
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
  const loadSequence = useRef(0);
  const loadAbort = useRef<AbortController | undefined>(undefined);
  const credentialSearchAbort = useRef<AbortController | undefined>(undefined);
  const scopeRef = useRef({ token, tenant });
  scopeRef.current = { token, tenant };
  const load = async () => {
    loadAbort.current?.abort();
    const controller = new AbortController();
    loadAbort.current = controller;
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken || !loadTenant) { setRoutes([]); setCredentials([]); return; }
    try {
      const [nextRoutes, nextCredentials] = await Promise.all([
        apiRead<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(loadTenant)}`, loadToken, { signal: controller.signal }),
        apiRead<KeyView[]>(`/internal/v1/keys${queryForTenant(loadTenant)}`, loadToken, { signal: controller.signal }),
      ]);
      if (sequence !== loadSequence.current || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      setRoutes(nextRoutes); setCredentials(nextCredentials); setError('');
    }
    catch (reason) { if (!controller.signal.aborted && sequence === loadSequence.current && scopeRef.current.token === loadToken && scopeRef.current.tenant === loadTenant) setError(messageOf(reason, t('common.requestFailed'))); }
  };
  const searchCredential = (query: string) => {
    credentialSearchAbort.current?.abort();
    const keyId = query.trim();
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(keyId) || !token || !tenant) return;
    const controller = new AbortController();
    credentialSearchAbort.current = controller;
    const searchToken = token; const searchTenant = tenant;
    const queryParameters = new URLSearchParams({ tenant_external_id: searchTenant, key_id: keyId, limit: '1' });
    void apiRead<KeyView[]>(`/internal/v1/keys?${queryParameters}`, searchToken, { signal: controller.signal }).then((matches) => {
      if (controller.signal.aborted || scopeRef.current.token !== searchToken || scopeRef.current.tenant !== searchTenant) return;
      setCredentials((current) => [...matches, ...current.filter((value) => !matches.some((match) => match.key_id === value.key_id))]);
      setError('');
    }).catch((reason) => {
      if (!controller.signal.aborted && scopeRef.current.token === searchToken && scopeRef.current.tenant === searchTenant) setError(messageOf(reason, t('common.requestFailed')));
    });
  };
  useEffect(() => {
    loadSequence.current += 1; setRoutes([]); setCredentials([]); setForm(emptyRouteDraft); setFormCatalog({ valid: false, allowCustom: false });
    setEditing(undefined); setEditForm(emptyRouteDraft); setEditCatalog({ valid: false, allowCustom: false });
    setBusy(''); setMessage(''); setError(''); void load();
    return () => { loadAbort.current?.abort(); credentialSearchAbort.current?.abort(); };
  }, [token, tenant]);
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
      {editing && <div className="inline-editor form-panel"><div className="panel-title"><h3>{t('routes.editTitle', { model: editing.public_model })}</h3><button type="button" className="secondary" onClick={() => setEditing(undefined)}>{t('common.cancel')}</button></div><RouteFields token={token} tenant={tenant} draft={editForm} upstreams={scopedUpstreams} providers={providers} providerGroups={providerGroups.groups} routeGroups={routeGroups.groups} credentials={credentials} onChange={setEditForm} onCatalogValidity={(valid, allowCustom) => setEditCatalog({ valid, allowCustom })} onCredentialQuery={searchCredential} /><button type="button" disabled={busy === editing.id || !canSubmit(editForm, editCatalog.valid)} onClick={() => void saveEdit()}>{t('common.save')}</button></div>}
    </article>
    <details className="panel create-resource"><summary><span><b>{t('routes.createTitle')}</b><small>{t('routes.description')}</small></span><span aria-hidden="true">＋</span></summary><div className="create-resource-body form-panel"><RouteFields token={token} tenant={tenant} draft={form} upstreams={scopedUpstreams} providers={providers} providerGroups={providerGroups.groups} routeGroups={routeGroups.groups} credentials={credentials} onChange={setForm} onCatalogValidity={(valid, allowCustom) => setFormCatalog({ valid, allowCustom })} onCredentialQuery={searchCredential} /><button type="button" disabled={busy === 'create' || !canSubmit(form, formCatalog.valid)} onClick={async () => { setBusy('create'); setMessage(''); setError(''); try { await api('/internal/v1/model-routes', token, { method: 'POST', body: JSON.stringify({ ...routeRequest(form, formCatalog.allowCustom), tenant_external_id: tenant }) }); setForm(emptyRouteDraft); setFormCatalog({ valid: false, allowCustom: false }); setMessage(t('routes.created')); await Promise.all([load(), routeGroups.load]); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}>{t('routes.create')}</button></div></details>
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
  const [grant, setGrant] = useState({ amount: '', source: '' });
  const [busy, setBusy] = useState('');
  const [newRouteIds, setNewRouteIds] = useState<string[]>([]);
  const [newRouteGroupIds, setNewRouteGroupIds] = useState<string[]>([]);
  const [groupFilter, setGroupFilter] = useState('all');
  const [secret, setSecret] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const loadSequence = useRef(0);
  const scopeRef = useRef({ token, tenant });
  scopeRef.current = { token, tenant };
  const credentialGroups = useGroups('credential', token, tenant);
  const routeGroups = useGroups('route', token, tenant);
  const createFormSchema = createSchema;
  const policyFormSchema = policySchema;
  const load = async () => {
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken || !loadTenant) { setValues([]); setRoutes([]); return; }
    try {
      const [nextValues, nextRoutes] = await Promise.all([
        api<KeyView[]>(`/internal/v1/keys${queryForTenant(loadTenant)}`, loadToken),
        api<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(loadTenant)}`, loadToken),
      ]);
      if (sequence !== loadSequence.current || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      setValues(nextValues); setRoutes(nextRoutes); setError('');
    }
    catch (reason) { if (sequence === loadSequence.current && scopeRef.current.token === loadToken && scopeRef.current.tenant === loadTenant) setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => {
    loadSequence.current += 1; setValues([]); setRoutes([]); setEditingPolicy(undefined); setEditingRouting(undefined); setRoutingDraft(undefined);
    setRenaming(undefined); setAliasDraft(''); setLimitSnapshots({}); setGranting(undefined); setGrant({ amount: '', source: '' }); setBusy('');
    setNewRouteIds([]); setNewRouteGroupIds([]); setGroupFilter('all'); setSecret(''); setMessage(''); setError(''); void load();
  }, [token, tenant]);
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
    const operationToken = token; const operationTenant = tenant;
    try {
      const routing = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing${queryForTenant(operationTenant)}`, operationToken);
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant) return;
      setRoutingDraft(routing); setEditingRouting(value.key_id);
    } catch (reason) { if (scopeRef.current.token === operationToken && scopeRef.current.tenant === operationTenant) setError(messageOf(reason, t('common.requestFailed'))); }
  };
  const saveRouting = async (value: KeyView, draft: CredentialRoutingView) => {
    const operationToken = token; const operationTenant = tenant;
    try {
      const saved = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing`, operationToken, { method: 'PUT', body: JSON.stringify({ tenant_external_id: operationTenant, route_ids: draft.route_ids, route_group_ids: draft.route_group_ids, expected_grant_revision: draft.grant_revision }) });
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant) return;
      setRoutingDraft(saved); setMessage(t('credentials.routingSaved')); setError('');
    } catch (reason) {
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant) return;
      if (reason instanceof ApiError && reason.status === 409) {
        const current = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing${queryForTenant(operationTenant)}`, operationToken);
        if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant) return;
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
          <div className="policy-chips"><span>{enumLabel(t, 'enforcementMode', value.policy.enforcement_mode)}</span><span>RPM {formatNumber(value.policy.requests_per_minute, locale)}</span><span>TPM {formatNumber(value.policy.tokens_per_minute, locale)}</span><span>{t('self.concurrency')} {formatNumber(value.policy.max_concurrency, locale)}</span><span>{t('budget.daily')}: {value.policy.daily_budget === null ? '—' : formatCurrency(value.policy.daily_budget, value.currency, locale)}</span><span>{t('budget.weekly')}: {value.policy.weekly_budget === null ? '—' : formatCurrency(value.policy.weekly_budget, value.currency, locale)}</span><span>{t('budget.lifetime')}: {value.policy.lifetime_budget === null ? '—' : formatCurrency(value.policy.lifetime_budget, value.currency, locale)}</span></div>
          <div className="row-actions"><button type="button" className="secondary" disabled={!tenant} onClick={() => { setRenaming(renaming === value.key_id ? undefined : value.key_id); setAliasDraft(value.alias); }}>{t('credentials.rename')}</button><button type="button" className="secondary" disabled={!tenant} onClick={async () => { try { const snapshot = await api<KeyLimitSnapshot>(`/internal/v1/keys/${value.key_id}/limits`, token); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setLimitSnapshots((current) => ({ ...current, [value.key_id]: snapshot })); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('credentials.viewLimits')}</button><button type="button" className="secondary" disabled={!tenant || value.status === 'revoked' || Boolean(busy)} onClick={async () => { if (!window.confirm(`${t('credentials.rotate')} · ${value.alias}\n${value.key_id}`)) return; setBusy(`rotate-${value.key_id}`); try { const result = await api<{ key: string }>(`/internal/v1/keys/${value.key_id}/rotate`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setSecret(result.key); setMessage(t('credentials.rotated', { alias: value.alias })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setBusy(''); } }}>{t('credentials.rotate')}</button><button type="button" className="secondary" disabled={!tenant || value.status === 'revoked'} onClick={() => setEditingPolicy(editingPolicy === value.key_id ? undefined : value.key_id)}>{t('credentials.editPolicy')}</button><button type="button" className="secondary" disabled={!tenant || value.status === 'revoked'} onClick={() => void openRouting(value)}>{t('credentials.routing')}</button><button type="button" className="secondary" disabled={!tenant || !value.account_id || value.status === 'revoked'} title={!value.account_id ? t('credentials.accountMissing') : undefined} onClick={() => setGranting(granting === value.key_id ? undefined : value.key_id)}>{t('credentials.grant')}</button>{value.status !== 'revoked' && <button type="button" className="secondary" disabled={!tenant} onClick={async () => { const nextStatus = value.status === 'active' ? 'suspended' : 'active'; try { await api(`/internal/v1/keys/${value.key_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setMessage(t(nextStatus === 'active' ? 'credentials.resumed' : 'credentials.suspended', { alias: value.alias })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } }}>{value.status === 'active' ? t('credentials.suspend') : t('credentials.resume')}</button>}</div>
          {renaming === value.key_id && <div className="inline-editor form-panel"><h3>{t('credentials.renameFor', { alias: value.alias })}</h3><label>{t('schema.Credential alias')}<input value={aliasDraft} maxLength={200} onChange={(event) => setAliasDraft(event.target.value)} /></label><button type="button" disabled={!aliasDraft.trim()} onClick={async () => { try { await api(`/internal/v1/keys/${value.key_id}/alias`, token, { method: 'PATCH', body: JSON.stringify({ alias: aliasDraft }) }); setRenaming(undefined); setMessage(t('credentials.renamed', { alias: aliasDraft.trim() })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('common.save')}</button></div>}
          {limitSnapshots[value.key_id] && <LimitSnapshot value={limitSnapshots[value.key_id]} />}
          {editingPolicy === value.key_id && policyFormSchema && <div className="inline-editor form-panel"><h3>{t('credentials.policyFor', { alias: value.alias })}</h3><Form key={`${value.key_id}-${locale}`} schema={localizeSchema(policyFormSchema as RJSFSchema, locale)} formData={value.policy} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { try { await api(`/internal/v1/keys/${value.key_id}/policy`, token, { method: 'PUT', body: JSON.stringify(formData) }); setEditingPolicy(undefined); setMessage(t('credentials.policySaved')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant}>{t('common.save')}</button></Form></div>}
          {editingRouting === value.key_id && routingDraft && <div className="inline-editor form-panel routing-editor"><h3>{t('credentials.routingFor', { alias: value.alias })}</h3><p className="muted">{t('credentials.routingHint')}</p>
            <MultiCombobox label={t('credentials.exactRoutes')} options={routeOptions} value={selections(routingDraft.route_ids, routeOptions)} onChange={(selected) => setRoutingDraft({ ...routingDraft, route_ids: selected.map((item) => item.value) })} placeholder={t('credentials.searchRoutes')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
            <MultiCombobox label={t('credentials.routeGroups')} options={routeGroupOptions} value={selections(routingDraft.route_group_ids, routeGroupOptions)} onChange={(selected) => setRoutingDraft({ ...routingDraft, route_group_ids: selected.map((item) => item.value) })} placeholder={t('credentials.searchRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('credentials.existingGroupsOnly')} />
            {routingDraft.effective_route_ids.length > 0 && <small className="field-hint">{t('credentials.effectiveRoutes', { count: formatNumber(routingDraft.effective_route_ids.length, locale) })}</small>}
            <button type="button" onClick={() => void saveRouting(value, routingDraft)}>{t('common.save')}</button>
          </div>}
          {granting === value.key_id && value.account_id && <div className="inline-editor form-panel"><h3>{t('credentials.grantFor', { alias: value.alias })}</h3><label>{t('credentials.grantAmount')} ({value.currency})<input inputMode="decimal" value={grant.amount} onChange={(event) => setGrant({ ...grant, amount: event.target.value })} /></label><label>{t('credentials.grantSource')}<input value={grant.source} onChange={(event) => setGrant({ ...grant, source: event.target.value })} /></label><button type="button" disabled={Boolean(busy) || !isPositiveDecimal(grant.amount) || !grant.source.trim()} onClick={async () => { const amount = grant.amount.trim(); const source = grant.source.trim(); if (!window.confirm(`${t('credentials.grantFor', { alias: value.alias })}\n${t('credentials.grantAmount')}: ${amount} ${value.currency}\n${t('credentials.grantSource')}: ${source}`)) return; setBusy(`grant-${value.key_id}`); try { await api(`/internal/v1/accounts/${value.account_id}/grants`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify({ amount, source }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setGranting(undefined); setGrant({ amount: '', source: '' }); setMessage(t('credentials.granted')); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setBusy(''); } }}>{t('credentials.confirmGrant')}</button></div>}
        </div>})}</div></article>
    <details className="panel create-resource"><summary><span><b>{t('credentials.createTitle')}</b><small>{t('credentials.createRoutingHint')}</small></span><span aria-hidden="true">＋</span></summary><div className="create-resource-body form-panel">
      <MultiCombobox label={t('credentials.exactRoutes')} options={routeOptions} value={selections(newRouteIds, routeOptions)} onChange={(selected) => setNewRouteIds(selected.map((item) => item.value))} placeholder={t('credentials.searchRoutes')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
      <MultiCombobox label={t('credentials.routeGroups')} options={routeGroupOptions} value={selections(newRouteGroupIds, routeGroupOptions)} onChange={(selected) => setNewRouteGroupIds(selected.map((item) => item.value))} placeholder={t('credentials.searchRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('credentials.existingGroupsOnly')} />
      {createFormSchema ? <Form key={`${tenant}-${locale}`} schema={localizeSchema(createFormSchema as RJSFSchema, locale)} uiSchema={{ tenant_external_id: { 'ui:widget': 'hidden' } }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try {
        const created = await api<{ key: string; key_id: string }>('/internal/v1/keys', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant, route_ids: newRouteIds, route_group_ids: newRouteGroupIds }) });
        if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return;
        setNewRouteIds([]); setNewRouteGroupIds([]); setSecret(created.key); setMessage(t(newRouteIds.length || newRouteGroupIds.length ? 'credentials.created' : 'credentials.createdNoRoutes')); await load();
      } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant}>{t('credentials.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
    </div></details>
  </section><GroupManager kind="credential" token={token} tenant={tenant} groups={credentialGroups.groups} resources={values.map((value) => ({ value: value.key_id, label: value.alias, description: value.key_id }))} onChanged={credentialGroups.load} /></>;
}

function ServiceCredentialWorkspace({ token, tenant, schema }: { token: string; tenant: string; schema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const [values, setValues] = useState<ServiceTokenView[]>([]);
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const loadSequence = useRef(0);
  const scopeRef = useRef({ token, tenant });
  scopeRef.current = { token, tenant };
  const load = async () => {
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken) { setValues([]); return; }
    try {
      const all = await api<ServiceTokenView[]>('/internal/v1/service-tokens', loadToken);
      if (sequence !== loadSequence.current || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      setValues(loadTenant ? all.filter((value) => value.tenant_external_id === loadTenant) : all); setError('');
    } catch (reason) { if (sequence === loadSequence.current && scopeRef.current.token === loadToken && scopeRef.current.tenant === loadTenant) setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => {
    loadSequence.current += 1; setValues([]); setSecret(''); setBusy(''); setMessage(''); setError(''); void load();
  }, [token, tenant]);
  return <>{!tenant && <div className="scope-context"><span aria-hidden="true">◎</span><p>{t('services.allTenantNotice')}</p></div>}{secret && <OneTimeSecret value={secret} message={t('services.oneTimeSecret')} />}<section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('services.title')}</h2><p className="muted">{t('services.description')}</p></div><span>{formatNumber(values.length, locale)}</span></div>{error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}<div className="account-list">{values.length === 0 && <div className="empty">{t('services.empty')}</div>}{values.map((value) => <div className="managed-resource" key={value.service_id}><div className="managed-resource-header"><div><b>{value.name}</b><small>{value.service_id}</small><span>{value.tenant_external_id ?? t('services.globalScope')} · {value.scopes.join(' · ')}</span></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : value.status === 'revoked' ? 'bad' : 'pending'}`}>{enumLabel(t, 'status', value.status ?? 'active')}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span></div></div><div className="row-actions"><button type="button" className="secondary" disabled={value.status === 'revoked' || Boolean(busy)} onClick={async () => { if (!window.confirm(`${t('services.rotate')} · ${value.name}\n${value.service_id}`)) return; setBusy(`rotate-${value.service_id}`); try { const result = await api<{ token: string }>(`/internal/v1/service-tokens/${value.service_id}/rotate`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setSecret(result.token); setMessage(t('services.rotated', { name: value.name })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setBusy(''); } }}>{t('services.rotate')}</button>{value.status !== 'revoked' && <button type="button" className="secondary" disabled={Boolean(busy)} onClick={async () => { const nextStatus = value.status === 'active' ? 'suspended' : 'active'; setBusy(`status-${value.service_id}`); try { await api(`/internal/v1/service-tokens/${value.service_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setMessage(t(nextStatus === 'active' ? 'services.resumed' : 'services.suspended', { name: value.name })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setBusy(''); } } }>{value.status === 'active' ? t('services.suspend') : t('services.resume')}</button>}</div></div>)}</div></article>
    <details className="panel create-resource"><summary><span><b>{t('services.createTitle')}</b><small>{t('services.description')}</small></span><span aria-hidden="true">＋</span></summary><div className="create-resource-body form-panel">{schema ? <Form key={`${tenant}-${locale}`} schema={localizeSchema(schema as RJSFSchema, locale)} uiSchema={{ tenant_external_id: { 'ui:widget': 'hidden' } }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!tenant) return; try { const created = await api<{ token: string }>('/internal/v1/service-tokens', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: tenant }) }); setSecret(created.token); setMessage(t('services.created')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!tenant}>{t('services.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}</div></details>
  </section></>;
}


interface OperatorPageProps { token: string; tenant: string }

function ResourceBoundary<T>({ resource, scopeKey, children }: {
  resource: ResourceState<T>;
  scopeKey: string;
  children: (value: T) => ReactNode;
}) {
  const { t } = useI18n();
  const previous = useRef<{ scopeKey: string; value: T } | undefined>(undefined);
  if (previous.current?.scopeKey !== scopeKey) previous.current = undefined;
  if (resource.kind === 'ready') previous.current = { scopeKey, value: resource.value };
  const value = resource.kind === 'ready' ? resource.value : previous.current?.value;
  if (!value) return resource.kind === 'failed'
    ? <div className="notice error" role="alert">{resource.message}</div>
    : <div className="empty">{t('common.loading')}</div>;
  return <>{resource.kind === 'ready' && resource.refreshError && <div className="notice error" role="alert">{resource.refreshError}</div>}{children(value)}</>;
}

export function ProvidersPage({ token, tenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    async () => {
      const [providers, values] = await Promise.all([
        api<ProviderType[]>('/internal/v1/provider-types', token),
        api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token),
      ]);
      return { providers, values };
    },
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={`${token}\0${tenant}`}>{({ providers, values }) =>
    <UpstreamProviders token={token} tenant={tenant} providers={providers} values={values} onChanged={resource.reload} />
  }</ResourceBoundary>;
}

export function PricingPage({ token, tenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<ConfigurationSchemas>('/internal/v1/schemas', token),
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={token}>{(schemas) =>
    <Pricing token={token} tenant={tenant} schemas={schemas} />
  }</ResourceBoundary>;
}

export function RoutesPage({ token, tenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    async () => {
      const [providers, upstreams] = await Promise.all([
        api<ProviderType[]>('/internal/v1/provider-types', token),
        api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token),
      ]);
      return { providers, upstreams };
    },
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={`${token}\0${tenant}`}>{({ providers, upstreams }) =>
    <RouteWorkspace token={token} tenant={tenant} providers={providers} upstreams={upstreams} />
  }</ResourceBoundary>;
}

export function CredentialsPage({ token, tenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<ConfigurationSchemas>('/internal/v1/schemas', token),
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={token}>{(schemas) =>
    <CredentialWorkspace token={token} tenant={tenant} createSchema={schemas.key_create} policySchema={schemas.key_policy} />
  }</ResourceBoundary>;
}

export function ServiceCredentialsPage({ token, tenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<ConfigurationSchemas>('/internal/v1/schemas', token),
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={token}>{(schemas) =>
    <ServiceCredentialWorkspace token={token} tenant={tenant} schema={schemas.service_token} />
  }</ResourceBoundary>;
}
