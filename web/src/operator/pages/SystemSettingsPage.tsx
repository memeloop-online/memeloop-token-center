import { useCallback, useEffect, useRef, useState } from 'react';
import { Button, Combobox, Option } from '@fluentui/react-components';
import { api } from '../../api';
import { CopyButton } from '../../CopyButton.js';
import { useI18n } from '../../i18n';
import { ModelPicker } from '../../ModelPicker';
import { SecretInput } from '../../SecretInput';
import type { FilterAssistantSettings } from '../../types';
import { assistantRouteCatalog, type ModelPickerProjectionItem, type ModelPickerProjectionPage } from '../modelCatalog';
import { messageOf } from '../scope/operatorShared';
import './systemSettings.css';

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
      <div><label htmlFor="operator-access-credential">{t('settings.accessCredential')}</label><SecretInput id="operator-access-credential" name="credential" label={t('settings.accessCredential')} autoComplete="off" value={credentialInput} onChange={(event) => onCredentialInput(event.target.value)} placeholder={t('operator.tokenPlaceholder')} /></div>
      <div className="button-row"><button type="submit" disabled={!credentialInput.trim()}>{credential ? t('settings.replaceCredential') : t('common.connect')}</button>{credentialInput.trim() && <CopyButton value={credentialInput} label={t('common.copySecret')} />}{credential && <><CopyButton value={credential} label={t('common.copySecret')} /><button type="button" className="secondary" onClick={onClear}>{t('common.clearCredential')}</button></>}</div>
    </form>
  </article>;
}

async function loadAssistantRouteCatalog(token: string, tenant: string): Promise<ModelPickerProjectionItem[]> {
  const data: ModelPickerProjectionItem[] = [];
  const seenCursors = new Set<string>();
  let cursor: string | null = null;
  do {
    const query = new URLSearchParams({ tenant_external_id: tenant, selection_kind: 'route', limit: '100' });
    if (cursor) query.set('cursor', cursor);
    const page = await api<ModelPickerProjectionPage>(`/internal/v1/model-picker-options?${query}`, token);
    if (page.contract_version !== 'model_picker_projection_v1' || !Array.isArray(page.data)) {
      throw new Error('Unexpected model-picker catalog contract');
    }
    data.push(...page.data);
    cursor = page.next_cursor;
    if (cursor && seenCursors.has(cursor)) throw new Error('Repeated model-picker catalog cursor');
    if (cursor) seenCursors.add(cursor);
  } while (cursor);
  return data;
}

/**
 * Tenant-scoped system policy for the natural-language filter assistant.
 * The API intentionally stores only a stable model-route UUID; it neither
 * fetches nor displays a client or upstream credential secret.
 */
export function SystemSettingsPage({ token, tenant }: { token: string; tenant: string; writeTenant?: string }) {
  return <AssistantSettings key={JSON.stringify([token, tenant])} token={token} tenant={tenant} />;
}

function AssistantSettings({ token, tenant }: { token: string; tenant: string }) {
  const { t } = useI18n();
  const [catalogItems, setCatalogItems] = useState<ModelPickerProjectionItem[]>([]);
  const [settings, setSettings] = useState<FilterAssistantSettings | null>();
  const [selectedRouteId, setSelectedRouteId] = useState('');
  const [billingChoices, setBillingChoices] = useState<BillingChoice[]>([]);
  const [billingCursor, setBillingCursor] = useState<BillingCursor | null>(null);
  const billingSequence = useRef(0);
  const [selectedBillingId, setSelectedBillingId] = useState('');
  const [billingLoading, setBillingLoading] = useState(false);
  const [billingError, setBillingError] = useState('');
  const [billingSearch, setBillingSearch] = useState('');
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [loadError, setLoadError] = useState('');
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const savingRequest = useRef<symbol | null>(null);
  const draftGeneration = useRef(0);
  const { options: assistantOptions, unverifiedRoutes } = assistantRouteCatalog(catalogItems);
  const selectedRouteIsVerified = !selectedRouteId || assistantOptions.some((option) => option.value === selectedRouteId);
  const selectedRouteHasAvailableCandidate = !selectedRouteId || assistantOptions.some((option) => option.value === selectedRouteId && !option.disabled);
  const selectedUnverifiedRoute = unverifiedRoutes.find((route) => route.value === selectedRouteId);

  const load = useCallback(async () => {
    const request = ++loadSequence.current;
    savingRequest.current = null; setSaving(false);
    setError(''); setLoadError(''); setMessage('');
    if (!token || !tenant) { setCatalogItems([]); setSettings(undefined); setLoading(false); return; }
    setLoading(true);
    try {
      const [nextCatalogItems, nextSettings] = await Promise.all([
        loadAssistantRouteCatalog(token, tenant),
        api<FilterAssistantSettings | null>(`/internal/v1/filter-assistant/settings?tenant_external_id=${encodeURIComponent(tenant)}`, token),
      ]);
      if (request !== loadSequence.current) return;
      setCatalogItems(nextCatalogItems); setSettings(nextSettings); setSelectedRouteId(nextSettings?.model_route_id ?? '');
      setSelectedBillingId(nextSettings?.billing_key_id ?? '');
    } catch (reason) {
      if (request !== loadSequence.current) return;
      const nextError = messageOf(reason, t('common.requestFailed'));
      setLoadError(nextError);
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
    if (savingRequest.current || loading || loadError || billingLoading || billingError || !tenant || !selectedRouteId || !selectedRouteHasAvailableCandidate || !billingChoices.some((choice) => choice.key_id === selectedBillingId)) return;
    const operation = Symbol();
    savingRequest.current = operation;
    const request = loadSequence.current;
    const draft = draftGeneration.current;
    const current = () => request === loadSequence.current && draft === draftGeneration.current;
    setSaving(true); setError(''); setMessage('');
    try {
      const next = await api<FilterAssistantSettings>('/internal/v1/filter-assistant/settings', token, {
        method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, model_route_id: selectedRouteId, billing_key_id: selectedBillingId, expected_updated_at: settings?.updated_at ?? null }),
      });
      if (current()) { setSettings(next); setMessage(t('settings.filterAssistantSaved')); }
    } catch (reason) { if (current()) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { if (savingRequest.current === operation) savingRequest.current = null; if (current()) setSaving(false); }
  };

  const changeRoute = (value: string) => {
    draftGeneration.current += 1;
    billingSequence.current += 1;
    setSelectedRouteId(value); setSelectedBillingId(''); setBillingSearch('');
    setBillingChoices([]); setBillingCursor(null); setBillingError(''); setMessage(''); setError('');
  };
  const billingLabel = (choice: BillingChoice) => `${choice.alias} · ${choice.principal}`;
  const visibleBilling = billingChoices.filter((choice) => billingLabel(choice).toLocaleLowerCase().includes(billingSearch.trim().toLocaleLowerCase()));

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
      <article className="settings-card" aria-busy={loading || saving}>
        <div className="settings-card-heading">
          <div>
            <h3>{t('settings.filterAssistantTitle')}</h3>
            <p className="muted">{t('settings.filterAssistantDescription')}</p>
          </div>
          {settings?.billing_key_id && <span className="status ok">{t('settings.configured')}</span>}
        </div>
        {loading ? <div className="empty" role="status">{t('common.loading')}</div> : loadError ? <div className="settings-empty" role="alert"><b>{t('settings.filterAssistantLoadFailed')}</b><span>{loadError}</span><button type="button" className="secondary" onClick={() => void load()}>{t('common.retry')}</button></div> : assistantOptions.length === 0 ? <div className="settings-empty"><b>{t('settings.noEnabledRoute')}</b><span>{t('settings.noEnabledRouteHint')}</span></div> : <form className="system-settings-form" onSubmit={(event) => { event.preventDefault(); void save(); }}>
          <div><ModelPicker label={t('settings.filterAssistantRoute')} popupLabel={t('settings.filterAssistantRoute')} value={selectedRouteIsVerified ? selectedRouteId : ''} onChange={changeRoute} disabled={saving} options={assistantOptions} describedBy="filter-assistant-route-hint" /><small id="filter-assistant-route-hint">{t('settings.assistantTextHint')}</small>{selectedRouteId && !selectedUnverifiedRoute && !selectedRouteHasAvailableCandidate && <small className="error-text" role="alert">{t('settings.assistantTextUnavailable')}</small>}</div>
          <div className="settings-billing-field"><label htmlFor="assistant-billing-choice">{t('settings.assistantBillingCredential')}</label><Combobox id="assistant-billing-choice" aria-describedby="assistant-billing-hint" placeholder={t('settings.assistantSelectBillingCredential')} value={billingSearch || billingChoices.filter((choice) => choice.key_id === selectedBillingId).map(billingLabel)[0] || ''} selectedOptions={selectedBillingId ? [selectedBillingId] : []} disabled={saving || !selectedRouteId} onChange={(event) => { setBillingSearch(event.target.value); setSelectedBillingId(''); draftGeneration.current += 1; setMessage(''); }} onOptionSelect={(_, data) => { setSelectedBillingId(data.optionValue ?? ''); setBillingSearch(''); draftGeneration.current += 1; setMessage(''); }}>
            {visibleBilling.map((choice) => <Option key={choice.key_id} value={choice.key_id} text={billingLabel(choice)}>{billingLabel(choice)}</Option>)}
            {!billingLoading && !billingError && visibleBilling.length === 0 && <Option disabled>{t(billingChoices.length ? 'settings.noMatchingCredential' : 'settings.assistantNoBillingCredential')}</Option>}
          </Combobox><small id="assistant-billing-hint">{t('settings.assistantBillingHint')}</small></div>
          {billingError && <div className="error-text" role="alert">{billingError}<button type="button" className="secondary" disabled={billingLoading} onClick={() => void loadBilling(billingCursor ?? undefined)}>{t('common.retry')}</button></div>}
          {billingLoading && <p role="status">{t('common.loading')}</p>}
          {billingCursor && <button type="button" className="secondary" disabled={billingLoading} onClick={() => void loadBilling(billingCursor)}>{t('settings.assistantMoreCredentials')}</button>}
          {!billingLoading && selectedRouteId && !billingError && !billingCursor && billingChoices.length === 0 && <p role="status">{t('settings.assistantNoBillingCredential')}</p>}
          <div className="button-row"><Button appearance="primary" type="submit" disabled={saving || billingLoading || Boolean(billingError) || !selectedRouteId || !selectedRouteIsVerified || !selectedRouteHasAvailableCandidate || !billingChoices.some((choice) => choice.key_id === selectedBillingId)}>{saving ? t('common.loading') : t('common.save')}</Button></div>
        </form>}
        {!loading && !loadError && selectedUnverifiedRoute && <p className="error-text" role="alert">{t('settings.assistantTextSelectedUnverified', { model: selectedUnverifiedRoute.label })}</p>}
        {!loading && !loadError && unverifiedRoutes.length > 0 && <p className="settings-status-note" role="status">{t('settings.assistantTextUnverified', { count: unverifiedRoutes.length })}</p>}
        {settings === null && !loadError && <p className="settings-status-note">{t('settings.filterAssistantNotConfigured')}</p>}
        {settings && !settings.billing_key_id && <p className="settings-status-note">{t('settings.assistantExecutionNotEnabled')}</p>}
      </article>
    </>}
  </section>;
}
