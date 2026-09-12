import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '../../api';
import { CopyButton } from '../../CopyButton.js';
import { useI18n } from '../../i18n';
import { ModelPicker } from '../../ModelPicker';
import type { FilterAssistantSettings, GroupView, ModelRouteView, UpstreamAccount } from '../../types';
import { routeModelOptions } from '../modelCatalog';
import { messageOf, queryForTenant } from '../scope/operatorShared';

type BillingChoice = { key_id: string; alias: string; principal: string };
type BillingCursor = { before_created_at: number; before_id: string };
type BillingPage = { data: BillingChoice[]; next_cursor: BillingCursor | null };

export interface OperatorAccessSettingsProps {
  credentialInput: string;
  credential: string;
  authenticating?: boolean;
  onCredentialInput: (value: string) => void;
  onConnect: (value: string) => void;
  onClear: () => void;
}

/**
 * The operator access form belongs to System settings. Keeping it as a
 * small controlled component lets the application shell keep authentication
 * state while avoiding a credential editor on every management page.
 */
export function OperatorAccessSettings({ credentialInput, credential, authenticating = false, onCredentialInput, onConnect, onClear }: OperatorAccessSettingsProps) {
  const { t } = useI18n();
  return <article className="panel settings-card settings-access-card">
    <div className="settings-card-heading">
      <div>
        <h3>{t('settings.accessTitle')}</h3>
        <p className="muted">{t('settings.accessDescription')}</p>
      </div>
      {credential && <span className="status ok">{t('common.savedCredentialInUse')}</span>}
    </div>
    <form className="system-settings-access operator-credential" aria-busy={authenticating} onSubmit={(event) => {
      event.preventDefault();
      const submittedCredential = new FormData(event.currentTarget).get('credential');
      if (typeof submittedCredential === 'string' && submittedCredential.trim()) onConnect(submittedCredential);
    }}>
      <label htmlFor="operator-access-credential">{t('settings.accessCredential')}<input id="operator-access-credential" name="credential" autoComplete="new-password" type="password" value={credentialInput} onChange={(event) => onCredentialInput(event.target.value)} placeholder={t('operator.tokenPlaceholder')} /></label>
      <div className="button-row"><button type="submit" disabled={!credentialInput.trim()}>{credential ? t('settings.replaceCredential') : t('common.connect')}</button>{credentialInput.trim() && <CopyButton value={credentialInput} label={t('common.copySecret')} />}{credential && <><CopyButton value={credential} label={t('common.copySecret')} /><button type="button" className="secondary" onClick={onClear}>{t('common.clearCredential')}</button></>}</div>
    </form>
  </article>;
}

/**
 * Tenant-scoped system policy for the natural-language filter assistant.
 * The API intentionally stores only a stable model-route UUID; it neither
 * fetches nor displays a client or upstream credential secret.
 */
export function SystemSettingsPage({ token, tenant }: { token: string; tenant: string; writeTenant?: string }) {
  const { t } = useI18n();
  const [routes, setRoutes] = useState<ModelRouteView[]>([]);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [settings, setSettings] = useState<FilterAssistantSettings | null>();
  const [selectedRouteId, setSelectedRouteId] = useState('');
  const [billingChoices, setBillingChoices] = useState<BillingChoice[]>([]);
  const [billingCursor, setBillingCursor] = useState<BillingCursor | null>(null);
  const billingSequence = useRef(0);
  const [selectedBillingId, setSelectedBillingId] = useState('');
  const [billingLoading, setBillingLoading] = useState(false);
  const [billingError, setBillingError] = useState('');
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [loadError, setLoadError] = useState('');
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const assistantOptions = routeModelOptions(routes, upstreams, groups, t('modelPicker.unknown'), 'route');
  const selectedRouteHasAvailableCandidate = !selectedRouteId || assistantOptions.some((option) => option.value === selectedRouteId && !option.disabled);

  const load = useCallback(async () => {
    const request = ++loadSequence.current;
    setError(''); setLoadError(''); setMessage('');
    if (!token || !tenant) { setRoutes([]); setSettings(undefined); setLoading(false); return; }
    setLoading(true);
    try {
      const [nextRoutes, nextSettings, nextUpstreams, nextGroups] = await Promise.all([
        api<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(tenant)}`, token),
        api<FilterAssistantSettings | null>(`/internal/v1/filter-assistant/settings?tenant_external_id=${encodeURIComponent(tenant)}`, token),
        api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token),
        api<GroupView[]>(`/internal/v1/provider-groups${queryForTenant(tenant)}`, token),
      ]);
      if (request !== loadSequence.current) return;
      const enabled = nextRoutes.filter((route) => route.enabled && ['openai', 'anthropic'].includes(route.protocol));
      setRoutes(enabled); setSettings(nextSettings); setSelectedRouteId(nextSettings?.model_route_id ?? '');
      setSelectedBillingId(nextSettings?.billing_key_id ?? '');
      setUpstreams(nextUpstreams); setGroups(nextGroups);
    } catch (reason) {
      if (request !== loadSequence.current) return;
      const nextError = messageOf(reason, t('common.requestFailed'));
      setLoadError(nextError); setError(nextError);
    } finally {
      if (request === loadSequence.current) setLoading(false);
    }
  }, [t, tenant, token]);

  useEffect(() => {
    setSelectedRouteId('');
    void load();
    return () => { loadSequence.current += 1; };
  }, [load]);

  const loadBilling = useCallback(async (cursor?: BillingCursor) => {
    const request = ++billingSequence.current;
    if (!cursor) { setBillingChoices([]); setBillingCursor(null); }
    setBillingError('');
    if (!token || !tenant || !selectedRouteId) { setBillingLoading(false); return; }
    setBillingLoading(true);
    try {
      const suffix = cursor ? `&before_created_at=${cursor.before_created_at}&before_id=${encodeURIComponent(cursor.before_id)}` : '';
      const page = await api<BillingPage>(`/internal/v1/filter-assistant/billing-choices?tenant_external_id=${encodeURIComponent(tenant)}&model_route_id=${encodeURIComponent(selectedRouteId)}${suffix}`, token);
      if (request !== billingSequence.current) return;
      setBillingChoices((current) => cursor ? [...current, ...page.data.filter((choice) => !current.some((previous) => previous.key_id === choice.key_id))] : page.data);
      setBillingCursor(page.next_cursor);
    } catch (reason) { if (request === billingSequence.current) setBillingError(messageOf(reason, t('common.requestFailed'))); }
    finally { if (request === billingSequence.current) setBillingLoading(false); }
  }, [selectedRouteId, t, tenant, token]);

  useEffect(() => { void loadBilling(); return () => { billingSequence.current += 1; }; }, [loadBilling]);

  const save = async () => {
    if (!tenant || !selectedRouteId || !billingChoices.some((choice) => choice.key_id === selectedBillingId)) return;
    setSaving(true); setError(''); setMessage('');
    try {
      const next = await api<FilterAssistantSettings>('/internal/v1/filter-assistant/settings', token, {
        method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, model_route_id: selectedRouteId, billing_key_id: selectedBillingId, expected_updated_at: settings?.updated_at ?? null }),
      });
      setSettings(next); setMessage(t('settings.filterAssistantSaved'));
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setSaving(false); }
  };

  return <section className="system-settings" aria-labelledby="system-settings-title">
    <header className="settings-page-header">
      <div>
        <span className="eyebrow">{t('settings.eyebrow')}</span>
        <h2 id="system-settings-title">{t('settings.title')}</h2>
        <p className="muted">{t('settings.description')}</p>
      </div>
    </header>
    {!tenant ? <article className="panel settings-card"><div className="empty">{t('settings.selectTenant')}</div></article> : <>
      {error && <div className="notice error" role="alert">{error}</div>}
      {message && <div className="notice success" role="status">{message}</div>}
      <article className="panel settings-card" aria-busy={loading || saving}>
        <div className="settings-card-heading">
          <div>
            <h3>{t('settings.filterAssistantTitle')}</h3>
            <p className="muted">{t('settings.filterAssistantDescription')}</p>
          </div>
          {settings?.billing_key_id && <span className="status ok">{t('settings.configured')}</span>}
        </div>
        {loading ? <div className="empty" role="status">{t('common.loading')}</div> : loadError ? <div className="settings-empty" role="alert"><b>{t('settings.filterAssistantLoadFailed')}</b><span>{loadError}</span><button type="button" className="secondary" onClick={() => void load()}>{t('common.retry')}</button></div> : routes.length === 0 ? <div className="settings-empty"><b>{t('settings.noEnabledRoute')}</b><span>{t('settings.noEnabledRouteHint')}</span></div> : <form className="system-settings-form" onSubmit={(event) => { event.preventDefault(); void save(); }}>
          <div><ModelPicker label={t('settings.filterAssistantRoute')} popupLabel={t('settings.filterAssistantRoute')} value={selectedRouteId} onChange={setSelectedRouteId} disabled={saving} options={assistantOptions} describedBy="filter-assistant-route-hint" /><small id="filter-assistant-route-hint">{t('settings.filterAssistantRouteHint')}</small>{selectedRouteId && !selectedRouteHasAvailableCandidate && <small className="error-text" role="alert">{t('settings.filterAssistantRouteUnavailable')}</small>}</div>
          <label>{t('settings.assistantBillingCredential')}<select value={selectedBillingId} disabled={saving || billingLoading} onChange={(event) => setSelectedBillingId(event.target.value)}><option value="">{t('settings.assistantSelectBillingCredential')}</option>{billingChoices.map((choice) => <option key={choice.key_id} value={choice.key_id}>{choice.alias} · {choice.principal}</option>)}</select><small>{t('settings.assistantBillingHint')}</small></label>
          {billingError && <div className="error-text" role="alert">{billingError}<button type="button" className="secondary" disabled={billingLoading} onClick={() => void loadBilling(billingCursor ?? undefined)}>{t('common.retry')}</button></div>}
          {billingLoading && <p role="status">{t('common.loading')}</p>}
          {billingCursor && <button type="button" className="secondary" disabled={billingLoading} onClick={() => void loadBilling(billingCursor)}>{t('settings.assistantMoreCredentials')}</button>}
          {!billingLoading && selectedRouteId && !billingError && !billingCursor && billingChoices.length === 0 && <p role="status">{t('settings.assistantNoBillingCredential')}</p>}
          <button type="submit" disabled={saving || billingLoading || !selectedRouteId || !selectedRouteHasAvailableCandidate || !billingChoices.some((choice) => choice.key_id === selectedBillingId)}>{saving ? t('common.loading') : t('common.save')}</button>
        </form>}
        {settings === null && !loadError && <p className="settings-status-note">{t('settings.filterAssistantNotConfigured')}</p>}
        {settings && !settings.billing_key_id && <p className="settings-status-note">{t('settings.assistantExecutionNotEnabled')}</p>}
      </article>
    </>}
  </section>;
}
