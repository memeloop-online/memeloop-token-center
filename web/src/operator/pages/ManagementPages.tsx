import { useConfirmDialog } from '../../useConfirmDialog';
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
  ConfigurationSchemas, CredentialRoutingView, GenerationPriceView, GroupView, KeyLimitSnapshot, KeyListCursor, KeyView,
  ModelPriceSyncResult, ModelPriceUsageSummary, ModelPriceView, ModelRouteView, ProviderType,
  OperatorMonitoringSnapshot, ServiceTokenView, UpstreamAccount, UpstreamDeletionReadiness, UpstreamHealth,
} from '../../types';
import { GroupManager, useGroups } from '../GroupManager';
import { MultiCombobox, type ComboboxOption } from '../MultiCombobox';
import { ResourceListStatusEmpty, ResourceListStatusFilterControl, useResourceListStatusFilter } from '../ResourceListStatusFilter';
import { UpstreamModelCombobox } from '../UpstreamModelCombobox';
import {
  applyKeyPage, canLoadMoreKeys, canReadCredentialLimits, canWriteCredential,
  credentialListPresentation, keyListPath, matchesCredentialSearch,
  ownsKeyListRequest, shouldLoadCredentialRoutes,
  type KeyListLoadState, type KeyListRequestIdentity,
} from '../keyPagination';
import { directCredentialSchema, supportsDirectConnection } from '../providerConnectionMethods';
import { UpstreamAvailability } from '../UpstreamAvailability';
import { upstreamAvailabilityPath, type UpstreamAvailabilityWindow } from '../upstreamAvailabilityWindow';
import { useOperatorResource, type ResourceState } from '../hooks/useOperatorResource';
import { enumLabel, messageOf, OneTimeSecret, queryForTenant, WriteScopeNotice } from '../scope/operatorShared';

function Form(props: FormProps) {
  return <RjsfForm {...props} noHtml5Validate onError={() => { /* Validation is rendered inline. */ }} />;
}

function recentAvailabilityPath(tenant: string, now: number) {
  const query = new URLSearchParams({
    scope: tenant ? 'tenant' : 'global',
    from_created_at: String(now - 86_400_000),
    to_created_at: String(now),
  });
  if (tenant) query.set('tenant_external_id', tenant);
  return `/internal/v1/monitoring-snapshot?${query}`;
}

function isPositiveDecimal(value: string) {
  const normalized = value.trim();
  return /^(?:\d+(?:\.\d+)?|\.\d+)$/.test(normalized) && /[1-9]/.test(normalized);
}

function UpstreamProviders({ token, tenant, writeTenant = tenant, providers, values, availabilitySnapshot, availabilityWindow, availabilityError, availabilityLoading, onOpenRequest, onChanged }: { token: string; tenant: string; writeTenant?: string; providers: ProviderType[]; values: UpstreamAccount[]; availabilitySnapshot?: OperatorMonitoringSnapshot; availabilityWindow?: UpstreamAvailabilityWindow; availabilityError?: string; availabilityLoading?: boolean; onOpenRequest?: (requestId: string) => void; onChanged: () => Promise<void> }) {
  const { locale, t } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const [method, setMethod] = useState<'direct' | 'authorization'>('direct');
  const [driver, setDriver] = useState('');
  const [rotating, setRotating] = useState<UpstreamAccount>();
  const [editing, setEditing] = useState<UpstreamAccount>();
  const [reauthorizing, setReauthorizing] = useState<UpstreamAccount>();
  const [busy, setBusy] = useState('');
  const [health, setHealth] = useState<Record<string, UpstreamHealth>>({});
  const [deletionReadiness, setDeletionReadiness] = useState<Record<string, UpstreamDeletionReadiness>>({});
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const providerGroups = useGroups('provider', token, writeTenant);
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
    setBusy(''); setHealth({}); setDeletionReadiness({}); setMessage(''); setError('');
  }, [token, tenant, writeTenant]);

  const statusFilter = useResourceListStatusFilter('upstreams', tenant, values, (value) => value.status === 'active');

  const canManage = (value: UpstreamAccount) => Boolean(writeTenant) && (!value.tenant_external_id || value.tenant_external_id === writeTenant);

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
    if (!canManage(value) || !await confirm(t('providers.confirmDisconnect', { name: value.name }))) return;
    setBusy(`disconnect-${value.id}`);
    setError(''); setMessage('');
    try {
      await api(`/internal/v1/upstreams/${value.id}/oauth/disconnect`, token, {
        method: 'POST',
        body: JSON.stringify({ tenant_external_id: writeTenant, expected_updated_at: value.updated_at }),
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
      await api(`/internal/v1/upstreams/${value.id}`, token, { method: 'PATCH', body: JSON.stringify({ tenant_external_id: writeTenant, status, expected_updated_at: value.updated_at }) });
      setHealth((current) => { const next = { ...current }; delete next[value.id]; return next; });
      setDeletionReadiness((current) => { const next = { ...current }; delete next[value.id]; return next; });
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
      const result = await api<UpstreamHealth>(`/internal/v1/upstreams/${value.id}/health${queryForTenant(writeTenant)}`, token, { method: 'POST' });
      setHealth((current) => ({ ...current, [value.id]: result }));
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  function deletionMessages(readiness: UpstreamDeletionReadiness) {
    const messages: string[] = [];
    if (readiness.requires_disabled) messages.push(t('providers.deleteRequiresDisabled'));
    if (readiness.model_route_count > 0) messages.push(t('providers.deleteBlockedRoutes', { count: formatNumber(readiness.model_route_count, locale) }));
    if (readiness.imported_for_audit) messages.push(t('providers.deleteBlockedImport'));
    if (readiness.can_delete) messages.push(t('providers.deleteReady'));
    return messages;
  }

  async function remove(value: UpstreamAccount) {
    if (!canManage(value)) return;
    setBusy(`delete-${value.id}`); setError(''); setMessage('');
    try {
      const readiness = await api<UpstreamDeletionReadiness>(`/internal/v1/upstreams/${value.id}/deletion-readiness${queryForTenant(writeTenant)}`, token);
      setDeletionReadiness((current) => ({ ...current, [value.id]: readiness }));
      if (!readiness.can_delete) {
        setError(deletionMessages(readiness).join(' '));
        return;
      }
      if (!await confirm(t('providers.confirmDelete', { name: value.name }))) return;
      const query = new URLSearchParams({ tenant_external_id: writeTenant, expected_updated_at: String(value.updated_at) });
      await api(`/internal/v1/upstreams/${value.id}?${query}`, token, { method: 'DELETE' });
      setMessage(t('providers.deleted', { name: value.name }));
      await onChanged();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  return <>{confirmationDialog}<WriteScopeNotice tenant={writeTenant} /><section className="provider-layout">
    <article className="panel provider-list"><div className="panel-title"><div><h2>{t('providers.title')}</h2><p className="muted">{t('providers.description')}</p></div><ResourceListStatusFilterControl filter={statusFilter} inactiveLabel={t('resourceList.inactive')} /></div>
      {error && <div className="notice error" role="alert">{error}</div>}{providerGroups.error && <div className="notice error" role="alert">{providerGroups.error}</div>}{availabilityError && <div className="notice error" role="alert">{availabilityError}</div>}{message && <div className="notice success" role="status">{message}</div>}
      <div className="account-list">{statusFilter.values.length === 0 && <ResourceListStatusEmpty totalCount={statusFilter.totalCount} normalLabel={t('status.active')} empty={t('providers.empty')} />}{statusFilter.values.map((value) => {
        const providerAvailable = providers.some((provider) => provider.id === value.driver);
        const currentHealth = providerAvailable ? health[value.id] : undefined;
        const manageable = canManage(value);
        const memberships = providerGroups.groups.filter((group) => group.member_ids.includes(value.id));
        const currentReadiness = deletionReadiness[value.id];
        const deletionBlockers = currentReadiness ? deletionMessages(currentReadiness) : [];
        return <div className="account provider-account" data-upstream-id={value.id} key={value.id}>
          <div className="account-main">
            <b>{value.name}</b>
            <span>{value.driver} · {t('providers.method')}: {enumLabel(t, 'auth', value.connection_method)}{value.tenant_external_id ? ` · ${value.tenant_external_id}` : ''}</span>
            {memberships.length > 0 && <div className="table-chip-list provider-group-summary" aria-label={t('groups.provider.title')}>{memberships.map((group) => <span key={group.id}>{group.name}</span>)}</div>}
            {!providerAvailable && <span className="pill">{t('providers.retired')}</span>}
            <small>{value.id}</small>
            {value.credential_expires_at && <small>{t('providers.expires')}: {new Date(value.credential_expires_at).toLocaleString(locale)}</small>}
            <UpstreamAvailability account={value} snapshot={availabilitySnapshot} window={availabilityWindow} loading={availabilityLoading} manualHealth={currentHealth} onOpenRequest={onOpenRequest} />
            {currentReadiness && <small className={`status ${currentReadiness.can_delete ? 'ok' : 'pending'}`}>{deletionBlockers.join(' · ')}</small>}
          </div>
          <div className="account-meta">
            <span className={`status ${value.status === 'active' ? 'ok' : 'pending'}`}>{enumLabel(t, 'status', value.status)}</span>
            <span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span>
            <span className="pill">{t('providers.routes', { count: formatNumber(value.route_count, locale) })}</span>
            <div className="row-actions">
              {providerAvailable && <>
                <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setEditing(value)}>{t('providers.edit')}</button>
                <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void checkHealth(value)}>{t('providers.runManualHealthCheck')}</button>
                {value.can_refresh && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void refreshOAuth(value)}>{t('providers.refreshAuthorization')}</button>}
                {value.can_reauthorize && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setReauthorizing(value)}>{t('providers.reauthorize')}</button>}
                {value.auth_kind === 'oauth' && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void disconnectOAuth(value)}>{t('providers.disconnect')}</button>}
                {value.can_rotate && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => setRotating(value)}>{t('providers.rotateCredential')}</button>}
              </>}
              {(value.status === 'active' || providerAvailable) && <button type="button" className="secondary" disabled={!manageable || Boolean(busy)} onClick={() => void setStatus(value, value.status === 'active' ? 'disabled' : 'active')}>{value.status === 'active' ? t('providers.disable') : t('providers.enable')}</button>}
              <button type="button" className="danger" title={deletionBlockers.length > 0 ? deletionBlockers.join(' ') : undefined} disabled={!manageable || Boolean(busy)} onClick={() => void remove(value)}>{t('common.remove')}</button>
            </div>
          </div>
        </div>;
      })}</div>
      {editing && editSchema && <div className="inline-editor"><div className="panel-title"><h3>{t('providers.editFor', { name: editing.name })}</h3><button type="button" className="secondary" onClick={() => setEditing(undefined)}>{t('common.cancel')}</button></div><Form key={`${editing.id}-${locale}`} schema={editSchema} uiSchema={{ config: { oauth: { 'ui:disabled': true } } }} formData={{ name: editing.name, config: editing.config }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!formData) return; setBusy(`edit-${editing.id}`); try { await api(`/internal/v1/upstreams/${editing.id}`, token, { method: 'PUT', body: JSON.stringify({ ...formData, tenant_external_id: writeTenant, expected_updated_at: editing.updated_at }) }); setEditing(undefined); setMessage(t('providers.updated', { name: editing.name })); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}><button type="submit" disabled={!canManage(editing) || Boolean(busy)}>{t('common.save')}</button></Form></div>}
      {rotating && rotateProvider && <div className="inline-editor"><div className="panel-title"><h3>{t('providers.rotateFor', { name: rotating.name })}</h3><button type="button" className="secondary" onClick={() => setRotating(undefined)}>{t('common.cancel')}</button></div><Form key={`${rotating.id}-${locale}`} schema={localizeSchema(rotateProvider.credential_schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { setBusy(`rotate-${rotating.id}`); try { await api(`/internal/v1/upstreams/${rotating.id}/credential`, token, { method: 'PUT', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify({ credential: formData }) }); setRotating(undefined); setMessage(t('providers.rotated', { name: rotating.name })); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}><button type="submit" disabled={!canManage(rotating) || Boolean(busy)}>{t('providers.confirmRotate')}</button></Form></div>}
    </article>
    <details key={reauthorizing?.id ?? 'provider-create'} className="panel create-resource provider-onboarding" open={reauthorizing ? true : undefined}><summary><span><b>{reauthorizing ? t('providers.reauthorizeFor', { name: reauthorizing.name }) : t('providers.add')}</b><small>{t('providers.description')}</small></span><span aria-hidden="true">＋</span></summary><div className="create-resource-body form-panel">{reauthorizing ? <>
      <div className="panel-title"><h2>{t('providers.reauthorizeFor', { name: reauthorizing.name })}</h2><button type="button" className="secondary" onClick={() => setReauthorizing(undefined)}>{t('common.cancel')}</button></div>
      <AuthorizationConnection key={`reauthorize-${reauthorizing.id}`} token={token} tenant={writeTenant} providers={providers} existing={reauthorizing} onChanged={async () => { setReauthorizing(undefined); setMessage(t('providers.reauthorized', { name: reauthorizing.name })); await onChanged(); }} />
    </> : <>
      <div className="segmented" role="group" aria-label={t('providers.method')}><button type="button" aria-pressed={method === 'direct'} className={method === 'direct' ? 'active' : ''} onClick={() => setMethod('direct')}>{t('providers.direct')}</button><button type="button" aria-pressed={method === 'authorization'} className={method === 'authorization' ? 'active' : ''} onClick={() => setMethod('authorization')}>{t('providers.oauth')}</button></div>
      {method === 'direct' ? <>
        <label>{t('providers.provider')}<select value={provider?.id ?? ''} onChange={(event) => setDriver(event.target.value)}>{directProviders.map((value) => <option key={value.id} value={value.id}>{value.display_name} · {value.source}</option>)}</select></label>
        {schema ? <Form key={`${provider.id}-${locale}`} schema={schema} uiSchema={uiSchema} fields={schemaFormFields} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!writeTenant) return; try { setError(''); await api('/internal/v1/upstreams', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: writeTenant }) }); setMessage(t('providers.created')); await onChanged(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!writeTenant || !token}>{t('providers.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
      </> : <AuthorizationConnection token={token} tenant={writeTenant} providers={providers} onChanged={onChanged} />}</>}</div>
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

function Pricing({ token, tenant, writeTenant = tenant, schemas }: { token: string; tenant: string; writeTenant?: string; schemas?: ConfigurationSchemas }) {
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
  const [usageFailed, setUsageFailed] = useState(false);
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const priceRequest = useRef<AbortController | undefined>(undefined);
  const syncSequence = useRef(0);
  const scopeRef = useRef({ token, tenant, writeTenant, displayCurrency });
  scopeRef.current = { token, tenant, writeTenant, displayCurrency };
  const load = async (requestedCurrency = displayCurrency) => {
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken) return;
    priceRequest.current?.abort();
    const controller = new AbortController();
    priceRequest.current = controller;
    const signal = AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]);
    const current = () => !controller.signal.aborted && sequence === loadSequence.current
      && scopeRef.current.token === loadToken && scopeRef.current.tenant === loadTenant
      && scopeRef.current.displayCurrency === requestedCurrency;
    setPricingLoading(true); setPrices([]); setGenerationPrices([]);
    // Publish each independent price table as soon as it arrives. Usage does
    // not vary by currency and must never gate price rendering.
    const results = await Promise.allSettled([
      api<ModelPriceView[]>(`/internal/v1/model-prices?currency=${encodeURIComponent(requestedCurrency)}`, loadToken, { signal })
        .then((value) => { if (current()) { setPrices(value); setLoadedCurrency(requestedCurrency); } }),
      api<GenerationPriceView[]>(`/internal/v1/generation-prices?currency=${encodeURIComponent(requestedCurrency)}`, loadToken, { signal })
        .then((value) => { if (current()) setGenerationPrices(value); }),
    ]);
    if (!current()) return;
    setLoadedCurrency(requestedCurrency); setPricingLoading(false);
    const failures = results.filter((result) => result.status === 'rejected');
    setError(failures.length ? t('pricing.partialLoad', { count: formatNumber(failures.length, locale) }) : '');
  };
  useEffect(() => {
    loadSequence.current += 1;
    syncSequence.current += 1;
    setPrices([]); setGenerationPrices([]); setSyncResult(undefined); setLoadedCurrency('');
    setPricingLoading(false); setSyncing(false); setError(''); setMessage(''); setKind('token'); setModel('');
  }, [token, tenant, writeTenant]);
  useEffect(() => {
    void load(displayCurrency);
    return () => { priceRequest.current?.abort(); loadSequence.current += 1; };
  }, [token, tenant, writeTenant, displayCurrency]);
  useEffect(() => {
    const controller = new AbortController();
    setUsage({ models: [] }); setUsageFailed(false);
    if (token) void api<ModelPriceUsageSummary>(`/internal/v1/model-prices/usage-summary${queryForTenant(tenant)}`, token, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]) })
      .then((value) => { if (!controller.signal.aborted) setUsage(value); })
      .catch(() => { if (!controller.signal.aborted) setUsageFailed(true); });
    return () => controller.abort();
  }, [token, tenant]);
  const renderCurrency = loadedCurrency || displayCurrency;
  const rows = useMemo(() => {
    const usageByModel = new Map(usage.models.map((value) => [value.model, value]));
    const pricesByModel = new Map(prices.map((value) => [value.model, value]));
    return Array.from(new Set([...usageByModel.keys(), ...pricesByModel.keys()])).sort().flatMap((name) => {
      const price = pricesByModel.get(name);
      const tiers = price?.tiers?.length ? price.tiers : price ? [{ service_tier: 'default', input_per_million: price.input_per_million, cached_input_per_million: price.input_per_million, cache_write_per_million: price.input_per_million, output_per_million: price.output_per_million, source: price.source, updated_at: price.updated_at, cache_price_estimated: true }] : [undefined];
      return tiers.map((tier, index) => ({ model: name, usage: index === 0 ? usageByModel.get(name) : undefined, tier }));
    });
  }, [prices, usage]);
  const schema = kind === 'generation' ? schemas?.generation_price : schemas?.model_price;
  const sync = async () => {
    if (!writeTenant) return;
    const syncToken = token; const syncTenant = tenant; const syncWriteTenant = writeTenant; const syncCurrency = displayCurrency;
    const sequence = ++syncSequence.current;
    // A pre-sync read must not overwrite newly synchronized prices.
    priceRequest.current?.abort(); loadSequence.current += 1; setPricingLoading(false);
    setSyncing(true); setError(''); setMessage('');
    try {
      const result = await api<ModelPriceSyncResult>('/internal/v1/model-prices/sync', syncToken, { method: 'POST', body: JSON.stringify({ models: usage.models.map((value) => value.model), currency: displayCurrency, tenant_external_id: syncWriteTenant }) });
      if (sequence !== syncSequence.current || scopeRef.current.token !== syncToken || scopeRef.current.tenant !== syncTenant || scopeRef.current.writeTenant !== syncWriteTenant || scopeRef.current.displayCurrency !== syncCurrency) return;
      setSyncResult(result); setPrices(result.prices); setLoadedCurrency(syncCurrency); setMessage(t('pricing.synced', { count: formatNumber(result.imported, locale) }));
    } catch (reason) { if (sequence === syncSequence.current && scopeRef.current.token === syncToken && scopeRef.current.tenant === syncTenant && scopeRef.current.writeTenant === syncWriteTenant && scopeRef.current.displayCurrency === syncCurrency) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { if (sequence === syncSequence.current && scopeRef.current.token === syncToken && scopeRef.current.tenant === syncTenant && scopeRef.current.writeTenant === syncWriteTenant && scopeRef.current.displayCurrency === syncCurrency) setSyncing(false); }
  };
  return <div className="pricing-page"><WriteScopeNotice tenant={writeTenant} />
    {usageFailed && <div className="notice error" role="alert">{t('pricing.partialLoad')}</div>}
    <article className="panel pricing-overview"><div className="panel-title"><div><h2>{t('pricing.title')}</h2><p className="muted">{t('pricing.description')}</p></div><div className="pricing-heading-actions"><label>{t('pricing.viewCurrency')}<select aria-label={t('pricing.viewCurrency')} value={displayCurrency} onChange={(event) => { const next = event.target.value; syncSequence.current += 1; setSyncing(false); setSyncResult(undefined); setMessage(''); setDisplayCurrency(next); setCurrency(next); }}><option value="USD">USD</option><option value="CNY">CNY</option></select></label><div className="disabled-action"><button type="button" onClick={() => void sync()} disabled={!writeTenant || syncing}>{syncing ? t('pricing.syncing') : t('pricing.sync')}</button></div></div></div>
      <div className="pricing-summary"><span>{t('pricing.usedModels', { count: formatNumber(usage.models.length, locale) })}</span><span>{t('pricing.saved', { count: formatNumber(prices.length, locale) })}</span><span>{t('pricing.sourceOrder')}: models.dev → LiteLLM → OpenRouter</span></div>
      {error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      {syncResult && <><div className="source-status">{syncResult.sourceResults.map((source) => <div className={`source-card ${source.error ? 'failed' : 'healthy'}`} key={source.source}><b>{source.source}</b><span>{source.error ? t('pricing.sourceFailed') : t('pricing.sourceHealthy', { count: formatNumber(source.models, locale) })}</span>{source.error && <small>{source.error}</small>}</div>)}</div><div className="notice success"><b>{t('pricing.result')}</b> · {t('pricing.imported', { count: formatNumber(syncResult.imported, locale) })} · {t('pricing.candidates', { count: formatNumber(syncResult.candidates.length, locale) })} · {t('pricing.unmatched', { count: formatNumber(syncResult.unmatched.length, locale) })} · {t('pricing.preserved', { count: formatNumber(syncResult.preserved.length, locale) })}</div>
        {(syncResult.candidates.length > 0 || syncResult.unmatched.length > 0) && <div className="sync-details"><h3>{t('pricing.candidateDetails')}</h3>{syncResult.candidates.map((candidate) => <details key={candidate.model}><summary><code>{candidate.model}</code><span>{t('pricing.candidateCount', { count: formatNumber(candidate.candidates.length, locale) })}</span></summary><div className="candidate-list">{candidate.candidates.map((match) => <div key={`${match.source}-${match.sourceModelId}-${match.serviceTier}`}><b>{match.sourceModelId}</b><span>{match.source} · {match.serviceTier} · {match.reason}</span><code>{t('pricing.input')}: {formatCurrency(match.inputPerMillion, renderCurrency, locale)} · {t('pricing.output')}: {formatCurrency(match.outputPerMillion, renderCurrency, locale)}</code></div>)}</div></details>)}{syncResult.unmatched.length > 0 && <details><summary>{t('pricing.unmatchedModels')}</summary><div className="model-name-list">{syncResult.unmatched.map((name) => <code key={name}>{name}</code>)}</div></details>}</div>}
      </>}
      <div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.calls')}</th><th>{t('pricing.serviceTier')}</th><th>{t('pricing.input')}</th><th>{t('pricing.cachedInput')}</th><th>{t('pricing.cacheWrite')}</th><th>{t('pricing.output')}</th><th>{t('pricing.source')}</th><th>{t('pricing.updated')}</th></tr></thead><tbody>{rows.map((row) => <tr key={`${row.model}-${row.tier?.service_tier ?? 'missing'}`}><td><code>{row.model}</code></td><td>{row.usage ? formatNumber(row.usage.calls, locale) : ''}</td><td>{row.tier?.service_tier ?? '—'}</td><td>{row.tier ? formatCurrency(row.tier.input_per_million, renderCurrency, locale) : '—'}</td><td>{row.tier ? <>{formatCurrency(row.tier.cached_input_per_million, renderCurrency, locale)}{row.tier.cache_price_estimated && <small className="muted"> {t('pricing.estimated')}</small>}</> : '—'}</td><td>{row.tier ? <>{formatCurrency(row.tier.cache_write_per_million, renderCurrency, locale)}{row.tier.cache_price_estimated && <small className="muted"> {t('pricing.estimated')}</small>}</> : '—'}</td><td>{row.tier ? formatCurrency(row.tier.output_per_million, renderCurrency, locale) : '—'}</td><td>{row.tier ? <span className={`pill source-${row.tier.source.replace('.', '-')}`}>{row.tier.source}</span> : <span className="status pending">{t('pricing.missing')}</span>}</td><td>{row.tier ? new Date(row.tier.updated_at).toLocaleString(locale) : '—'}</td></tr>)}</tbody></table>{rows.length === 0 && <div className="empty">{pricingLoading ? t('common.loading') : t('pricing.noPricesForCurrency', { currency: renderCurrency })}</div>}</div>
    </article>
    <article className="panel"><div className="panel-title"><h2>{t('pricing.generationPrices')}</h2><span>{formatNumber(generationPrices.length, locale)}</span></div><div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.currency')}</th><th>{t('self.units')}</th><th>{t('pricing.unitPrice')}</th></tr></thead><tbody>{generationPrices.map((price) => <tr key={`${price.currency}-${price.model}`}><td><code>{price.model}</code></td><td>{price.currency}</td><td>{enumLabel(t, 'billingUnit', price.billing_unit)}</td><td>{formatCurrency(price.price_per_unit, price.currency, locale)}</td></tr>)}</tbody></table>{generationPrices.length === 0 && <div className="empty">{t('pricing.noGenerationPrices')}</div>}</div></article>
    <details className="panel manual-pricing"><summary><span><b>{t('pricing.manual')}</b><small>{t('pricing.manualHint')}</small></span><span>＋</span></summary><div className="manual-pricing-body form-panel"><label>{t('pricing.type')}<select value={kind} onChange={(event) => setKind(event.target.value as typeof kind)}><option value="token">{t('pricing.tokenModel')}</option><option value="generation">{t('pricing.generationModel')}</option></select></label><label>{t('pricing.model')}<input value={model} onChange={(event) => setModel(event.target.value)} /></label><label>{t('pricing.currency')}<select value={currency} onChange={(event) => setCurrency(event.target.value)}><option value="USD">USD</option><option value="CNY">CNY</option></select></label>{schema ? <Form key={`${kind}-${locale}`} schema={localizeSchema(schema as RJSFSchema, locale)} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!writeTenant) return; try { const prefix = kind === 'generation' ? 'generation-prices' : 'prices'; await api(`/internal/v1/${prefix}/${encodeURIComponent(currency)}/${encodeURIComponent(model)}`, token, { method: 'POST', body: JSON.stringify(formData) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return; setMessage(t('pricing.savedMessage')); if (currency === displayCurrency) await load(currency); else setDisplayCurrency(currency); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!writeTenant || !model.trim()}>{t('pricing.save')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}</div></details>
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
    <UpstreamModelCombobox token={token} tenant={tenant} upstreams={upstreams} accountIds={draft.upstream_account_ids} includedProviderGroupIds={draft.included_provider_group_ids} excludedProviderGroupIds={draft.excluded_provider_group_ids} syncAccountIds={candidateIds} protocol={draft.protocol} value={draft.upstream_model} onChange={(upstream_model) => onChange({ ...draft, upstream_model, custom_model_confirmed: false })} customModelConfirmed={draft.custom_model_confirmed} onValidityChange={onCatalogValidity} />
    <label>{t('routes.priority')}<input type="number" min={-1000000} max={1000000} value={draft.priority} onChange={(event) => onChange({ ...draft, priority: Number(event.target.value) })} /></label>
    <MultiCombobox label={t('routes.routeGroups')} options={routeGroupOptions} value={routeGroupValue} onChange={(selected) => onChange({ ...draft, route_group_ids: selected.filter((item) => !item.created).map((item) => item.value), route_group_names: selected.filter((item) => item.created).map((item) => item.label) })} placeholder={t('routes.searchOrCreateRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} allowCreate createLabel={(name) => t('routes.createRouteGroupNamed', { name })} hint={t('routes.routeGroupsHint')} />
    <MultiCombobox label={t('routes.exactCredentials')} options={credentialOptions} value={selections(draft.granted_credential_ids, credentialOptions)} onChange={(selected) => onChange({ ...draft, granted_credential_ids: selected.map((item) => item.value) })} placeholder={t('routes.searchCredentials')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('routes.exactCredentialsHint')} onQueryChange={onCredentialQuery} />
  </>;
}

function RouteWorkspace({ token, tenant, writeTenant = tenant, upstreams, providers }: { token: string; tenant: string; writeTenant?: string; upstreams: UpstreamAccount[]; providers: ProviderType[] }) {
  const { locale, t } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const [routes, setRoutes] = useState<ModelRouteView[]>([]);
  const [credentials, setCredentials] = useState<KeyView[]>([]);
  const providerGroups = useGroups('provider', token, writeTenant);
  const routeGroups = useGroups('route', token, writeTenant);
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
  const scopeRef = useRef({ token, tenant, writeTenant });
  scopeRef.current = { token, tenant, writeTenant };
  const load = async () => {
    loadAbort.current?.abort();
    const controller = new AbortController();
    loadAbort.current = controller;
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken) { setRoutes([]); setCredentials([]); return; }
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
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(keyId) || !token || !writeTenant) return;
    const controller = new AbortController();
    credentialSearchAbort.current = controller;
    const searchToken = token; const searchTenant = tenant; const searchWriteTenant = writeTenant;
    const queryParameters = new URLSearchParams({ tenant_external_id: searchWriteTenant, key_id: keyId, limit: '1' });
    void apiRead<KeyView[]>(`/internal/v1/keys?${queryParameters}`, searchToken, { signal: controller.signal }).then((matches) => {
      if (controller.signal.aborted || scopeRef.current.token !== searchToken || scopeRef.current.tenant !== searchTenant || scopeRef.current.writeTenant !== searchWriteTenant) return;
      setCredentials((current) => [...matches, ...current.filter((value) => !matches.some((match) => match.key_id === value.key_id))]);
      setError('');
    }).catch((reason) => {
      if (!controller.signal.aborted && scopeRef.current.token === searchToken && scopeRef.current.tenant === searchTenant && scopeRef.current.writeTenant === searchWriteTenant) setError(messageOf(reason, t('common.requestFailed')));
    });
  };
  useEffect(() => {
    loadSequence.current += 1; setRoutes([]); setCredentials([]); setForm(emptyRouteDraft); setFormCatalog({ valid: false, allowCustom: false });
    setEditing(undefined); setEditForm(emptyRouteDraft); setEditCatalog({ valid: false, allowCustom: false });
    setBusy(''); setMessage(''); setError(''); void load();
    return () => { loadAbort.current?.abort(); credentialSearchAbort.current?.abort(); };
  }, [token, tenant, writeTenant]);
  const statusFilter = useResourceListStatusFilter('model-routes', tenant, routes, (route) => route.enabled);
  const scopedUpstreams = upstreams.filter((value) => !value.tenant_external_id || value.tenant_external_id === writeTenant);
  const canManage = (route: ModelRouteView) => Boolean(writeTenant) && route.tenant_external_id === writeTenant;
  const canSubmit = (draft: RouteDraft, catalogValid: boolean) => Boolean(writeTenant && catalogValid && draft.public_model.trim() && draft.upstream_model.trim()
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
      await api(`/internal/v1/model-routes/${editing.id}`, token, { method: 'PUT', body: JSON.stringify({ ...routeRequest(editForm, editCatalog.allowCustom), tenant_external_id: writeTenant, expected_updated_at: editing.updated_at, expected_grant_revision: editing.grant_revision }) });
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
      await api(`/internal/v1/model-routes/${route.id}`, token, { method: 'PATCH', body: JSON.stringify({ tenant_external_id: writeTenant, enabled, expected_updated_at: route.updated_at }) });
      setEditing(undefined); setMessage(t(enabled ? 'routes.enabled' : 'routes.disabled')); await load();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  };
  const remove = async (route: ModelRouteView) => {
    if (route.enabled || !await confirm(t('routes.confirmDelete', { model: route.public_model }))) return;
    const query = new URLSearchParams({ tenant_external_id: writeTenant, expected_updated_at: String(route.updated_at) });
    setBusy(route.id); setMessage(''); setError('');
    try {
      await api(`/internal/v1/model-routes/${route.id}?${query}`, token, { method: 'DELETE' });
      setEditing(undefined); setMessage(t('routes.deleted')); await load();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  };
  return <>{confirmationDialog}<WriteScopeNotice tenant={writeTenant} /><section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('routes.title')}</h2><p className="muted">{t('routes.description')}</p></div><ResourceListStatusFilterControl filter={statusFilter} inactiveLabel={t('resourceList.inactive')} /></div>{error && <div className="notice error" role="alert">{error}</div>}{providerGroups.error && <div className="notice error" role="alert">{providerGroups.error}</div>}{routeGroups.error && <div className="notice error" role="alert">{routeGroups.error}</div>}{message && <div className="notice success" role="status">{message}</div>}<div className="table-scroll"><table><thead><tr>{!tenant && <th>{t('credentials.tenant')}</th>}<th>{t('routes.publicModel')}</th><th>{t('routes.upstream')}</th><th>{t('routes.groups')}</th><th>{t('routes.upstreamModel')}</th><th>{t('routes.protocol')}</th><th>{t('routes.priority')}</th><th>{t('request.status')}</th><th>{t('routes.actions')}</th></tr></thead><tbody>{statusFilter.values.map((route) => <tr key={route.id}>{!tenant && <td><code>{route.tenant_external_id ?? '—'}</code></td>}<td><code>{route.public_model}</code></td><td><div className="table-chip-list">{(route.upstream_account_ids ?? (route.upstream_account_id ? [route.upstream_account_id] : [])).map((id) => <span key={id}>{upstreams.find((value) => value.id === id)?.name ?? id}</span>)}</div></td><td><div className="table-chip-list">{(route.route_group_ids ?? []).map((id) => <span key={id}>{routeGroups.groups.find((value) => value.id === id)?.name ?? id}</span>)}</div></td><td><code>{route.upstream_model}</code></td><td>{route.protocol}</td><td>{formatNumber(route.priority, locale)}</td><td><span className={`status ${route.enabled ? 'ok' : 'pending'}`}>{route.enabled ? t('common.enabled') : t('common.disabled')}</span></td><td><div className="row-actions"><button type="button" className="secondary" disabled={busy === route.id || !canManage(route)} onClick={() => beginEdit(route)}>{t('routes.edit')}</button><button type="button" className="secondary" disabled={busy === route.id || !canManage(route)} onClick={() => void setEnabled(route, !route.enabled)}>{route.enabled ? t('routes.disable') : t('routes.enable')}</button><button type="button" className="danger" title={route.enabled ? t('routes.disableBeforeDelete') : undefined} disabled={busy === route.id || !canManage(route) || route.enabled} onClick={() => void remove(route)}>{t('common.remove')}</button></div></td></tr>)}</tbody></table>{statusFilter.values.length === 0 && <ResourceListStatusEmpty totalCount={statusFilter.totalCount} normalLabel={t('common.enabled')} empty={t('routes.empty')} />}</div>
      {editing && <div className="inline-editor form-panel"><div className="panel-title"><h3>{t('routes.editTitle', { model: editing.public_model })}</h3><button type="button" className="secondary" onClick={() => setEditing(undefined)}>{t('common.cancel')}</button></div><RouteFields token={token} tenant={writeTenant} draft={editForm} upstreams={scopedUpstreams} providers={providers} providerGroups={providerGroups.groups} routeGroups={routeGroups.groups} credentials={credentials} onChange={setEditForm} onCatalogValidity={(valid, allowCustom) => setEditCatalog({ valid, allowCustom })} onCredentialQuery={searchCredential} /><button type="button" disabled={busy === editing.id || !canSubmit(editForm, editCatalog.valid)} onClick={() => void saveEdit()}>{t('common.save')}</button></div>}
    </article>
    <details className="panel create-resource"><summary><span><b>{t('routes.createTitle')}</b><small>{t('routes.description')}</small></span><span aria-hidden="true">＋</span></summary><div className="create-resource-body form-panel"><RouteFields token={token} tenant={writeTenant} draft={form} upstreams={scopedUpstreams} providers={providers} providerGroups={providerGroups.groups} routeGroups={routeGroups.groups} credentials={credentials} onChange={setForm} onCatalogValidity={(valid, allowCustom) => setFormCatalog({ valid, allowCustom })} onCredentialQuery={searchCredential} /><button type="button" disabled={busy === 'create' || !canSubmit(form, formCatalog.valid)} onClick={async () => { setBusy('create'); setMessage(''); setError(''); try { await api('/internal/v1/model-routes', token, { method: 'POST', body: JSON.stringify({ ...routeRequest(form, formCatalog.allowCustom), tenant_external_id: writeTenant }) }); setForm(emptyRouteDraft); setFormCatalog({ valid: false, allowCustom: false }); setMessage(t('routes.created')); await Promise.all([load(), routeGroups.load]); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } finally { setBusy(''); } }}>{t('routes.create')}</button></div></details>
  </section><section className="routing-group-managers">
    <GroupManager kind="provider" token={token} tenant={writeTenant} groups={providerGroups.groups} resources={scopedUpstreams.map((value) => ({ value: value.id, label: value.name, description: value.driver }))} onChanged={providerGroups.load} />
    <GroupManager kind="route" token={token} tenant={writeTenant} groups={routeGroups.groups} resources={routes.filter(canManage).map((route) => ({ value: route.id, label: route.public_model, description: route.protocol }))} onChanged={async () => { await Promise.all([routeGroups.load(), load()]); }} />
  </section></>;
}

function CredentialWorkspace({ token, tenant, writeTenant = tenant, createSchema, policySchema }: { token: string; tenant: string; writeTenant?: string; createSchema?: Record<string, unknown>; policySchema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
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
  const [search, setSearch] = useState('');
  const [nextCursor, setNextCursor] = useState<KeyListCursor>();
  const [keyListState, setKeyListState] = useState<KeyListLoadState>('idle');
  const [keyError, setKeyError] = useState('');
  const [routeError, setRouteError] = useState('');
  const [secret, setSecret] = useState('');
  const [manualRecoverySecret, setManualRecoverySecret] = useState<{ keyId: string; key: string }>();
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const scopeGeneration = useRef(0);
  const keyRequestGeneration = useRef(0);
  const routeRequestGeneration = useRef(0);
  const keyRequest = useRef<{ identity: KeyListRequestIdentity; controller: AbortController } | undefined>(undefined);
  const routeRequest = useRef<{ generation: number; scopeGeneration: number; controller: AbortController } | undefined>(undefined);
  const scopeRef = useRef({ token, tenant, writeTenant });
  scopeRef.current = { token, tenant, writeTenant };
  const credentialGroups = useGroups('credential', token, writeTenant);
  const routeGroups = useGroups('route', token, writeTenant);
  const createFormSchema = createSchema;
  const policyFormSchema = policySchema;
  const ownsKeyRequest = (request: { identity: KeyListRequestIdentity; controller: AbortController }) => ownsKeyListRequest(keyRequest.current?.identity, request.identity)
    && scopeGeneration.current === request.identity.scopeGeneration
    && scopeRef.current.token === token && scopeRef.current.tenant === tenant;
  const startKeyRequest = (state: Extract<KeyListLoadState, 'initial-loading' | 'loading-more'>) => {
    keyRequest.current?.controller.abort();
    const request = {
      identity: { generation: ++keyRequestGeneration.current, scopeGeneration: scopeGeneration.current },
      controller: new AbortController(),
    };
    keyRequest.current = request;
    setKeyListState(state);
    return request;
  };
  const loadRoutes = async (loadToken: string, loadTenant: string, currentScopeGeneration: number) => {
    routeRequest.current?.controller.abort();
    routeRequest.current = undefined;
    if (!shouldLoadCredentialRoutes(loadTenant)) { setRoutes([]); setRouteError(''); return; }
    const request = {
      generation: ++routeRequestGeneration.current,
      scopeGeneration: currentScopeGeneration,
      controller: new AbortController(),
    };
    routeRequest.current = request;
    setRouteError('');
    try {
      const nextRoutes = await apiRead<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(loadTenant)}`, loadToken, { signal: request.controller.signal });
      const active = routeRequest.current;
      if (!active || active.generation !== request.generation || active.scopeGeneration !== request.scopeGeneration || scopeGeneration.current !== request.scopeGeneration || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      routeRequest.current = undefined;
      setRoutes(nextRoutes); setRouteError('');
    } catch (reason) {
      const active = routeRequest.current;
      if (request.controller.signal.aborted || !active || active.generation !== request.generation || active.scopeGeneration !== request.scopeGeneration || scopeGeneration.current !== request.scopeGeneration || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      routeRequest.current = undefined;
      setRouteError(messageOf(reason, t('common.requestFailed')));
    }
  };
  const load = async () => {
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken) {
      keyRequest.current?.controller.abort(); keyRequest.current = undefined;
      routeRequest.current?.controller.abort(); routeRequest.current = undefined;
      setValues([]); setRoutes([]); setNextCursor(undefined); setKeyListState('idle'); setKeyError(''); setRouteError('');
      return;
    }
    const request = startKeyRequest('initial-loading');
    setKeyError('');
    void loadRoutes(loadToken, loadTenant, scopeGeneration.current);
    try {
      const keyRows = await apiRead<KeyView[]>(keyListPath(loadTenant), loadToken, { signal: request.controller.signal });
      if (!ownsKeyRequest(request) || request.controller.signal.aborted) return;
      const page = applyKeyPage([], keyRows);
      keyRequest.current = undefined;
      if (!page.ok) {
        setNextCursor(undefined); setKeyListState('failed'); setKeyError(t('credentials.paginationStalled'));
        return;
      }
      setValues(page.values); setNextCursor(page.nextCursor); setKeyListState(page.nextCursor ? 'more' : 'complete');
    }
    catch (reason) {
      if (!ownsKeyRequest(request) || request.controller.signal.aborted) return;
      keyRequest.current = undefined;
      setKeyListState('failed'); setKeyError(messageOf(reason, t('common.requestFailed')));
    }
  };
  useEffect(() => {
    scopeGeneration.current += 1; setValues([]); setRoutes([]); setEditingPolicy(undefined); setEditingRouting(undefined); setRoutingDraft(undefined);
    setRenaming(undefined); setAliasDraft(''); setLimitSnapshots({}); setGranting(undefined); setGrant({ amount: '', source: '' }); setBusy('');
    setNewRouteIds([]); setNewRouteGroupIds([]); setGroupFilter('all'); setSearch(''); setNextCursor(undefined); setKeyListState('initial-loading'); setKeyError(''); setRouteError(''); setSecret(''); setManualRecoverySecret(undefined); setMessage(''); setError(''); void load();
    return () => { keyRequest.current?.controller.abort(); routeRequest.current?.controller.abort(); };
  }, [token, tenant, writeTenant]);
  const loadMore = async () => {
    if (!canLoadMoreKeys(keyListState, Boolean(nextCursor), Boolean(keyRequest.current)) || !nextCursor || !token) return;
    const loadToken = token; const loadTenant = tenant; const cursor = nextCursor;
    const request = startKeyRequest('loading-more');
    setKeyError('');
    try {
      const keyRows = await apiRead<KeyView[]>(keyListPath(loadTenant, cursor), loadToken, { signal: request.controller.signal });
      if (!ownsKeyRequest(request) || request.controller.signal.aborted) return;
      const page = applyKeyPage(values, keyRows, cursor);
      keyRequest.current = undefined;
      if (!page.ok) {
        setNextCursor(undefined); setKeyListState('failed'); setKeyError(t('credentials.paginationStalled'));
        return;
      }
      setValues(page.values); setNextCursor(page.nextCursor); setKeyListState(page.nextCursor ? 'more' : 'complete');
    } catch (reason) {
      if (!ownsKeyRequest(request) || request.controller.signal.aborted) return;
      keyRequest.current = undefined;
      setKeyListState('failed'); setKeyError(messageOf(reason, t('common.requestFailed')));
    }
  };
  const nonStatusFilteredValues = values.filter((value) => {
    if (!matchesCredentialSearch(value, search, locale)) return false;
    if (groupFilter === 'all' || !writeTenant) return true;
    const memberships = credentialGroups.groups.filter((group) => group.member_ids.includes(value.key_id));
    return groupFilter === 'unassigned' ? memberships.length === 0 : memberships.some((group) => group.id === groupFilter);
  });
  const statusFilter = useResourceListStatusFilter('credentials', tenant, nonStatusFilteredValues, (value) => (value.status ?? 'active') === 'active');
  const filteredValues = statusFilter.values;
  const filtersApplied = Boolean(search.trim()) || !statusFilter.showInactive || groupFilter !== 'all';
  const loadingKeys = keyListState === 'initial-loading' || keyListState === 'loading-more';
  const canLoadMore = canLoadMoreKeys(keyListState, Boolean(nextCursor), Boolean(keyRequest.current));
  const listPresentation = credentialListPresentation(keyListState, filtersApplied);
  const canReadLimits = canReadCredentialLimits(token);
  const canWrite = canWriteCredential(writeTenant);
  const canManage = (value: KeyView) => canWrite && value.tenant_external_id === writeTenant;
  const copyRecoveredCredential = async (value: KeyView) => {
    if (!canManage(value) || value.status !== 'active' || !value.credential_recovery_available) return;
    const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant;
    setBusy(`copy-${value.key_id}`); setError(''); setMessage(''); setManualRecoverySecret(undefined);
    try {
      const result = await api<{ key_id: string; credential_generation: number; key: string }>(`/internal/v1/keys/${value.key_id}/credential-recovery/copy`, operationToken, { method: 'POST' });
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || scopeRef.current.writeTenant !== operationWriteTenant) return;
      if (result.key_id !== value.key_id || result.credential_generation !== value.credential_generation) throw new Error(t('common.requestFailed'));
      try {
        if (!navigator.clipboard?.writeText) throw new Error('Clipboard API unavailable');
        await navigator.clipboard.writeText(result.key);
        if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || scopeRef.current.writeTenant !== operationWriteTenant) return;
        setMessage(t('credentials.copySuccess', { alias: value.alias }));
      } catch {
        if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || scopeRef.current.writeTenant !== operationWriteTenant) return;
        // The recovered value remains only in this component state so the
        // authorized operator can manually copy it. It is never persisted.
        setManualRecoverySecret({ keyId: value.key_id, key: result.key });
      }
    } catch (reason) {
      if (scopeRef.current.token === operationToken && scopeRef.current.tenant === operationTenant && scopeRef.current.writeTenant === operationWriteTenant) setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      if (scopeRef.current.token === operationToken && scopeRef.current.tenant === operationTenant && scopeRef.current.writeTenant === operationWriteTenant) setBusy('');
    }
  };
  const routeOptions = routes.filter((route) => route.tenant_external_id === writeTenant).map((route) => ({ value: route.id, label: route.public_model, description: route.protocol }));
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
    const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant;
    try {
      const saved = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing`, operationToken, { method: 'PUT', body: JSON.stringify({ tenant_external_id: operationWriteTenant, route_ids: draft.route_ids, route_group_ids: draft.route_group_ids, expected_grant_revision: draft.grant_revision }) });
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || scopeRef.current.writeTenant !== operationWriteTenant) return;
      setRoutingDraft(saved); setMessage(t('credentials.routingSaved')); setError('');
    } catch (reason) {
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || scopeRef.current.writeTenant !== operationWriteTenant) return;
      if (reason instanceof ApiError && reason.status === 409) {
        const current = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing${queryForTenant(operationTenant)}`, operationToken);
        if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || scopeRef.current.writeTenant !== operationWriteTenant) return;
        setRoutingDraft(current); setError(t('credentials.concurrentRoutingReloaded'));
      } else setError(messageOf(reason, t('common.requestFailed')));
    }
  };
  return <>{confirmationDialog}<WriteScopeNotice tenant={writeTenant} />{secret && <OneTimeSecret value={secret} message={t('credentials.oneTimeSecret')} />}<section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('credentials.title')}</h2><p className="muted">{t('credentials.description')}</p></div><ResourceListStatusFilterControl filter={statusFilter} inactiveLabel={t('resourceList.inactive')} /></div>
      <div className="credential-list-controls"><label>{t('credentials.search')}<input type="search" value={search} onChange={(event) => setSearch(event.target.value)} placeholder={t('credentials.searchPlaceholder')} /></label>{writeTenant && <label>{t('credentials.groupFilter')}<select value={groupFilter} onChange={(event) => setGroupFilter(event.target.value)}><option value="all">{t('common.all')}</option><option value="unassigned">{t('credentials.ungrouped')}</option>{credentialGroups.groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}</select></label>}</div>
      <p className="credential-list-summary" role="status">{listPresentation === 'loading' ? t('credentials.loadingList') : listPresentation === 'loading-more' ? t('credentials.loadingMore', { count: formatNumber(values.length, locale) }) : listPresentation === 'failed' ? t('credentials.loadFailed', { count: formatNumber(values.length, locale) }) : listPresentation === 'filtered' ? t('credentials.filteredLoaded', { shown: formatNumber(filteredValues.length, locale), loaded: formatNumber(values.length, locale) }) : listPresentation === 'more' ? t('credentials.loadedMore', { count: formatNumber(values.length, locale) }) : t('credentials.loadedComplete', { count: formatNumber(values.length, locale) })}</p>
      {keyError && <div className="notice error" role="alert">{keyError}</div>}{routeError && <div className="notice error" role="alert">{routeError}</div>}{error && <div className="notice error" role="alert">{error}</div>}{credentialGroups.error && <div className="notice error" role="alert">{credentialGroups.error}</div>}{routeGroups.error && <div className="notice error" role="alert">{routeGroups.error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      <div className="account-list">{filteredValues.length === 0 && (loadingKeys ? <div className="empty">{t('common.loading')}</div> : keyListState === 'failed' ? <div className="empty">{t('credentials.loadFailed', { count: formatNumber(values.length, locale) })}</div> : <ResourceListStatusEmpty totalCount={statusFilter.totalCount} normalLabel={t('status.active')} empty={values.length === 0 ? t('credentials.empty') : t('credentials.noFilterResults')} />)}{filteredValues.map((value) => {
        const memberships = credentialGroups.groups.filter((group) => group.member_ids.includes(value.key_id));
        return <div className="managed-resource" key={value.key_id}><div className="managed-resource-header"><div><b>{value.alias}</b><small>{value.key_id}</small><span>{!tenant && <>{t('credentials.tenant')}: {value.tenant_external_id ?? '—'} · </>}{value.principal_external_id ?? t('common.unknownPrincipal')} · {formatCurrency(value.available_balance, value.currency, locale)}</span></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : value.status === 'revoked' ? 'bad' : 'pending'}`}>{enumLabel(t, 'status', value.status ?? 'active')}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span></div></div>
          <div className="credential-recovery-control">
            {value.credential_recovery_available && value.status === 'active'
              ? <button type="button" className="secondary" disabled={!canManage(value) || Boolean(busy)} onClick={() => void copyRecoveredCredential(value)}>{busy === `copy-${value.key_id}` ? t('common.loading') : t('credentials.copy')}</button>
              : <span className="pill credential-recovery-unavailable" title={t(value.status === 'revoked' ? 'credentials.copyRevoked' : value.status === 'suspended' ? 'credentials.copySuspended' : 'credentials.copyNotStored')}>{t(value.status === 'revoked' ? 'credentials.copyRevoked' : value.status === 'suspended' ? 'credentials.copySuspended' : 'credentials.copyNotStored')}</span>}
            {manualRecoverySecret?.keyId === value.key_id && <div className="credential-recovery-manual" role="status">
              <b>{t('credentials.copyManual')}</b>
              <code>{manualRecoverySecret.key}</code>
              <button type="button" className="secondary" onClick={() => setManualRecoverySecret(undefined)}>{t('common.close')}</button>
            </div>}
          </div>
          {memberships.length > 0 && <div className="table-chip-list credential-group-chips" aria-label={t('groups.credential.title')}>{memberships.map((group) => <span key={group.id}>{group.name}</span>)}</div>}
          <div className="policy-chips"><span>{enumLabel(t, 'enforcementMode', value.policy.enforcement_mode)}</span><span>RPM {formatNumber(value.policy.requests_per_minute, locale)}</span><span>TPM {formatNumber(value.policy.tokens_per_minute, locale)}</span><span>{t('self.concurrency')} {formatNumber(value.policy.max_concurrency, locale)}</span><span>{t('budget.daily')}: {value.policy.daily_budget === null ? '—' : formatCurrency(value.policy.daily_budget, value.currency, locale)}</span><span>{t('budget.weekly')}: {value.policy.weekly_budget === null ? '—' : formatCurrency(value.policy.weekly_budget, value.currency, locale)}</span><span>{t('budget.lifetime')}: {value.policy.lifetime_budget === null ? '—' : formatCurrency(value.policy.lifetime_budget, value.currency, locale)}</span></div>
          <div className="row-actions"><button type="button" className="secondary" disabled={!canWrite} onClick={() => { setRenaming(renaming === value.key_id ? undefined : value.key_id); setAliasDraft(value.alias); }}>{t('credentials.rename')}</button><button type="button" className="secondary" disabled={!canReadLimits} onClick={async () => { try { const snapshot = await api<KeyLimitSnapshot>(`/internal/v1/keys/${value.key_id}/limits`, token); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setLimitSnapshots((current) => ({ ...current, [value.key_id]: snapshot })); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('credentials.viewLimits')}</button><button type="button" className="secondary" disabled={!canWrite || value.status === 'revoked' || Boolean(busy)} onClick={async () => { if (!await confirm(`${t('credentials.rotate')} · ${value.alias}\n${value.key_id}`)) return; setBusy(`rotate-${value.key_id}`); try { const result = await api<{ key: string }>(`/internal/v1/keys/${value.key_id}/rotate`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setSecret(result.key); setMessage(t('credentials.rotated', { alias: value.alias })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setBusy(''); } }}>{t('credentials.rotate')}</button><button type="button" className="secondary" disabled={!canWrite || value.status === 'revoked'} onClick={() => setEditingPolicy(editingPolicy === value.key_id ? undefined : value.key_id)}>{t('credentials.editPolicy')}</button><button type="button" className="secondary" disabled={!canWrite || value.status === 'revoked'} onClick={() => void openRouting(value)}>{t('credentials.routing')}</button><button type="button" className="secondary" disabled={!canWrite || !value.account_id || value.status === 'revoked'} title={!value.account_id ? t('credentials.accountMissing') : undefined} onClick={() => setGranting(granting === value.key_id ? undefined : value.key_id)}>{t('credentials.grant')}</button>{value.status !== 'revoked' && <button type="button" className="secondary" disabled={!canWrite} onClick={async () => { const nextStatus = value.status === 'active' ? 'suspended' : 'active'; try { await api(`/internal/v1/keys/${value.key_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setMessage(t(nextStatus === 'active' ? 'credentials.resumed' : 'credentials.suspended', { alias: value.alias })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } }}>{value.status === 'active' ? t('credentials.suspend') : t('credentials.resume')}</button>}</div>
          {renaming === value.key_id && <div className="inline-editor form-panel"><h3>{t('credentials.renameFor', { alias: value.alias })}</h3><label>{t('schema.Credential alias')}<input value={aliasDraft} maxLength={200} onChange={(event) => setAliasDraft(event.target.value)} /></label><button type="button" disabled={!canWrite || !aliasDraft.trim()} onClick={async () => { try { await api(`/internal/v1/keys/${value.key_id}/alias`, token, { method: 'PATCH', body: JSON.stringify({ alias: aliasDraft }) }); setRenaming(undefined); setMessage(t('credentials.renamed', { alias: aliasDraft.trim() })); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}>{t('common.save')}</button></div>}
          {limitSnapshots[value.key_id] && <LimitSnapshot value={limitSnapshots[value.key_id]} />}
          {editingPolicy === value.key_id && policyFormSchema && <div className="inline-editor form-panel"><h3>{t('credentials.policyFor', { alias: value.alias })}</h3><Form key={`${value.key_id}-${locale}`} schema={localizeSchema(policyFormSchema as RJSFSchema, locale)} formData={value.policy} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { try { await api(`/internal/v1/keys/${value.key_id}/policy`, token, { method: 'PUT', body: JSON.stringify(formData) }); setEditingPolicy(undefined); setMessage(t('credentials.policySaved')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!canWrite}>{t('common.save')}</button></Form></div>}
          {editingRouting === value.key_id && routingDraft && <div className="inline-editor form-panel routing-editor"><h3>{t('credentials.routingFor', { alias: value.alias })}</h3><p className="muted">{t('credentials.routingHint')}</p>
            <MultiCombobox label={t('credentials.exactRoutes')} options={routeOptions} value={selections(routingDraft.route_ids, routeOptions)} onChange={(selected) => setRoutingDraft({ ...routingDraft, route_ids: selected.map((item) => item.value) })} placeholder={t('credentials.searchRoutes')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
            <MultiCombobox label={t('credentials.routeGroups')} options={routeGroupOptions} value={selections(routingDraft.route_group_ids, routeGroupOptions)} onChange={(selected) => setRoutingDraft({ ...routingDraft, route_group_ids: selected.map((item) => item.value) })} placeholder={t('credentials.searchRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('credentials.existingGroupsOnly')} />
            {routingDraft.effective_route_ids.length > 0 && <small className="field-hint">{t('credentials.effectiveRoutes', { count: formatNumber(routingDraft.effective_route_ids.length, locale) })}</small>}
            <button type="button" disabled={!canWrite} onClick={() => void saveRouting(value, routingDraft)}>{t('common.save')}</button>
          </div>}
          {granting === value.key_id && value.account_id && <div className="inline-editor form-panel"><h3>{t('credentials.grantFor', { alias: value.alias })}</h3><label>{t('credentials.grantAmount')} ({value.currency})<input inputMode="decimal" value={grant.amount} onChange={(event) => setGrant({ ...grant, amount: event.target.value })} /></label><label>{t('credentials.grantSource')}<input value={grant.source} onChange={(event) => setGrant({ ...grant, source: event.target.value })} /></label><button type="button" disabled={!canWrite || Boolean(busy) || !isPositiveDecimal(grant.amount) || !grant.source.trim()} onClick={async () => { const amount = grant.amount.trim(); const source = grant.source.trim(); if (!await confirm(`${t('credentials.grantFor', { alias: value.alias })}\n${t('credentials.grantAmount')}: ${amount} ${value.currency}\n${t('credentials.grantSource')}: ${source}`)) return; setBusy(`grant-${value.key_id}`); try { await api(`/internal/v1/accounts/${value.account_id}/grants`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify({ amount, source }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setGranting(undefined); setGrant({ amount: '', source: '' }); setMessage(t('credentials.granted')); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setBusy(''); } }}>{t('credentials.confirmGrant')}</button></div>}
        </div>})}</div>{keyListState === 'failed' ? <div className="load-more"><button type="button" className="secondary" onClick={() => void load()}>{t('credentials.retryLoad')}</button></div> : nextCursor && (keyListState === 'more' || keyListState === 'loading-more') && <div className="load-more"><button type="button" className="secondary" disabled={!canLoadMore} onClick={() => void loadMore()}>{loadingKeys ? t('common.loading') : t('credentials.loadMore')}</button></div>}</article>
    <details className="panel create-resource"><summary><span><b>{t('credentials.createTitle')}</b><small>{t('credentials.createRoutingHint')}</small></span><span aria-hidden="true">＋</span></summary><div className="create-resource-body form-panel">
      <MultiCombobox label={t('credentials.exactRoutes')} options={routeOptions} value={selections(newRouteIds, routeOptions)} onChange={(selected) => setNewRouteIds(selected.map((item) => item.value))} placeholder={t('credentials.searchRoutes')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
      <MultiCombobox label={t('credentials.routeGroups')} options={routeGroupOptions} value={selections(newRouteGroupIds, routeGroupOptions)} onChange={(selected) => setNewRouteGroupIds(selected.map((item) => item.value))} placeholder={t('credentials.searchRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('credentials.existingGroupsOnly')} />
      {createFormSchema ? <Form key={`${tenant}-${writeTenant}-${locale}`} schema={localizeSchema(createFormSchema as RJSFSchema, locale)} uiSchema={{ tenant_external_id: { 'ui:widget': 'hidden' } }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!writeTenant) return; try {
        const created = await api<{ key: string; key_id: string }>('/internal/v1/keys', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: writeTenant, route_ids: newRouteIds, route_group_ids: newRouteGroupIds }) });
        if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return;
        setNewRouteIds([]); setNewRouteGroupIds([]); setSecret(created.key); setMessage(t(newRouteIds.length || newRouteGroupIds.length ? 'credentials.created' : 'credentials.createdNoRoutes')); await load();
      } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!canWrite}>{t('credentials.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
    </div></details>
  </section>{writeTenant && <GroupManager kind="credential" token={token} tenant={writeTenant} groups={credentialGroups.groups} resources={values.filter(canManage).map((value) => ({ value: value.key_id, label: value.alias, description: value.key_id }))} onChanged={credentialGroups.load} />}</>;
}

function ServiceCredentialWorkspace({ token, tenant, writeTenant = tenant, schema }: { token: string; tenant: string; writeTenant?: string; schema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const [values, setValues] = useState<ServiceTokenView[]>([]);
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const loadSequence = useRef(0);
  const scopeRef = useRef({ token, tenant, writeTenant });
  scopeRef.current = { token, tenant, writeTenant };
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
  }, [token, tenant, writeTenant]);
  const statusFilter = useResourceListStatusFilter('service-credentials', tenant, values, (value) => (value.status ?? 'active') === 'active');
  const canManage = (value: ServiceTokenView) => Boolean(writeTenant) && (!value.tenant_external_id || value.tenant_external_id === writeTenant);
  return <>{confirmationDialog}{secret && <OneTimeSecret value={secret} message={t('services.oneTimeSecret')} />}<section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('services.title')}</h2><p className="muted">{t('services.description')}</p></div><ResourceListStatusFilterControl filter={statusFilter} inactiveLabel={t('resourceList.inactive')} /></div>{error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}<div className="account-list">{statusFilter.values.length === 0 && <ResourceListStatusEmpty totalCount={statusFilter.totalCount} normalLabel={t('status.active')} empty={t('services.empty')} />}{statusFilter.values.map((value) => <div className="managed-resource" key={value.service_id}><div className="managed-resource-header"><div><b>{value.name}</b><small>{value.service_id}</small><span>{value.tenant_external_id ?? t('services.globalScope')} · {value.scopes.join(' · ')}</span></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : value.status === 'revoked' ? 'bad' : 'pending'}`}>{enumLabel(t, 'status', value.status ?? 'active')}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span></div></div><div className="row-actions"><button type="button" className="secondary" disabled={!canManage(value) || value.status === 'revoked' || Boolean(busy)} onClick={async () => { if (!await confirm(`${t('services.rotate')} · ${value.name}\n${value.service_id}`)) return; setBusy(`rotate-${value.service_id}`); try { const result = await api<{ token: string }>(`/internal/v1/service-tokens/${value.service_id}/rotate`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return; setSecret(result.token); setMessage(t('services.rotated', { name: value.name })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setBusy(''); } }}>{t('services.rotate')}</button>{value.status !== 'revoked' && <button type="button" className="secondary" disabled={!canManage(value) || Boolean(busy)} onClick={async () => { const nextStatus = value.status === 'active' ? 'suspended' : 'active'; setBusy(`status-${value.service_id}`); try { await api(`/internal/v1/service-tokens/${value.service_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return; setMessage(t(nextStatus === 'active' ? 'services.resumed' : 'services.suspended', { name: value.name })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setBusy(''); } } }>{value.status === 'active' ? t('services.suspend') : t('services.resume')}</button>}</div></div>)}</div></article>
    <details className="panel create-resource"><summary><span><b>{t('services.createTitle')}</b><small>{t('services.description')}</small></span><span aria-hidden="true">＋</span></summary><div className="create-resource-body form-panel">{schema ? <Form key={`${tenant}-${writeTenant}-${locale}`} schema={localizeSchema(schema as RJSFSchema, locale)} uiSchema={{ tenant_external_id: { 'ui:widget': 'hidden' } }} validator={validator} templates={schemaFormTemplates} onSubmit={async ({ formData }) => { if (!writeTenant) return; try { const created = await api<{ token: string }>('/internal/v1/service-tokens', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: writeTenant }) }); setSecret(created.token); setMessage(t('services.created')); await load(); } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); } }}><button type="submit" disabled={!writeTenant}>{t('services.create')}</button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}</div></details>
  </section></>;
}


interface OperatorPageProps {
  token: string;
  /** Selected read scope; empty only when no tenant is available. */
  tenant: string;
  /** Explicit target for every create/update action. */
  writeTenant?: string;
}

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

export function ProvidersPage({ token, tenant, writeTenant, onOpenRequest }: OperatorPageProps & { onOpenRequest?: (requestId: string) => void }) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    async (signal) => {
      const [providers, values] = await Promise.all([
        api<ProviderType[]>('/internal/v1/provider-types', token, { signal: AbortSignal.any([signal, AbortSignal.timeout(10_000)]) }),
        api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token, { signal: AbortSignal.any([signal, AbortSignal.timeout(10_000)]) }),
      ]);
      return { providers, values };
    },
    t('common.requestFailed'),
  );
  // Statistics are independent: a slow aggregation must not hold the account
  // list, create forms, or OAuth actions behind a four-request waterfall.
  const statistics = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    async (signal) => {
      const now = Date.now();
      const [availability, windowResult] = await Promise.all([
        api<OperatorMonitoringSnapshot>(recentAvailabilityPath(tenant, now), token, { signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]) })
          .then((availabilitySnapshot) => ({ availabilitySnapshot, availabilityError: undefined }))
          .catch((reason) => ({ availabilitySnapshot: undefined, availabilityError: messageOf(reason, t('providers.availabilityUnavailable')) })),
        tenant ? api<UpstreamAvailabilityWindow>(upstreamAvailabilityPath(tenant, now), token, { signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]) })
          .then((availabilityWindow) => ({ availabilityWindow, windowError: undefined }))
          .catch((reason) => ({ availabilityWindow: undefined, windowError: messageOf(reason, t('providers.availabilityUnavailable')) }))
          : Promise.resolve({ availabilityWindow: undefined, windowError: t('providers.accountWindowSelectTenant') }),
      ]);
      return { ...availability, availabilityWindow: windowResult.availabilityWindow, availabilityError: windowResult.windowError ?? availability.availabilityError };
    },
    t('common.requestFailed'),
  );
  const availability = statistics.state.kind === 'ready' ? statistics.state.value : {
    availabilityError: statistics.state.kind === 'failed' ? statistics.state.message : undefined,
    availabilityLoading: statistics.state.kind === 'idle' || statistics.state.kind === 'loading',
  };
  return <ResourceBoundary resource={resource.state} scopeKey={`${token}\0${tenant}`}>{({ providers, values }) =>
    <UpstreamProviders token={token} tenant={tenant} writeTenant={writeTenant} providers={providers} values={values} {...availability} onOpenRequest={onOpenRequest} onChanged={async () => { await Promise.all([resource.reload(), statistics.reload()]); }} />
  }</ResourceBoundary>;
}

export function PricingPage({ token, tenant, writeTenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<ConfigurationSchemas>('/internal/v1/schemas', token, { signal: AbortSignal.timeout(10_000) }),
    t('common.requestFailed'),
  );
  // Schemas are only needed by the manual editor, not the price tables.
  // Keep it mounted while schema discovery completes or fails.
  return <>
    {resource.state.kind === 'failed' && <div className="notice error" role="alert">{resource.state.message}</div>}
    <Pricing key={`${token}\0${tenant}`} token={token} tenant={tenant} writeTenant={writeTenant} schemas={resource.state.kind === 'ready' ? resource.state.value : undefined} />
  </>;
}

export function RoutesPage({ token, tenant, writeTenant }: OperatorPageProps) {
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
    <RouteWorkspace token={token} tenant={tenant} writeTenant={writeTenant} providers={providers} upstreams={upstreams} />
  }</ResourceBoundary>;
}

export function CredentialsPage({ token, tenant, writeTenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<ConfigurationSchemas>('/internal/v1/schemas', token),
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={token}>{(schemas) =>
    <CredentialWorkspace token={token} tenant={tenant} writeTenant={writeTenant} createSchema={schemas.key_create} policySchema={schemas.key_policy} />
  }</ResourceBoundary>;
}

export function ServiceCredentialsPage({ token, tenant, writeTenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<ConfigurationSchemas>('/internal/v1/schemas', token),
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={token}>{(schemas) =>
    <ServiceCredentialWorkspace token={token} tenant={tenant} writeTenant={writeTenant} schema={schemas.service_token} />
  }</ResourceBoundary>;
}
