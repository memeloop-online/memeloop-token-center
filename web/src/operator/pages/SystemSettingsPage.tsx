import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '../../api';
import { CopyButton } from '../../CopyButton.js';
import { useI18n } from '../../i18n';
import { ModelPicker } from '../../ModelPicker';
import type { FilterAssistantSettings } from '../../types';
import { assistantRouteCatalog, type ModelPickerProjectionItem, type ModelPickerProjectionPage } from '../modelCatalog';
import { messageOf } from '../scope/operatorShared';

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
  const { t } = useI18n();
  const [catalogItems, setCatalogItems] = useState<ModelPickerProjectionItem[]>([]);
  const [settings, setSettings] = useState<FilterAssistantSettings | null>();
  const [selectedRouteId, setSelectedRouteId] = useState('');
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [loadError, setLoadError] = useState('');
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const { options: assistantOptions, unverifiedRoutes } = assistantRouteCatalog(catalogItems);
  const selectedRouteIsVerified = !selectedRouteId || assistantOptions.some((option) => option.value === selectedRouteId);
  const selectedRouteHasAvailableCandidate = !selectedRouteId || assistantOptions.some((option) => option.value === selectedRouteId && !option.disabled);
  const selectedUnverifiedRoute = unverifiedRoutes.find((route) => route.value === selectedRouteId);

  const load = useCallback(async () => {
    const request = ++loadSequence.current;
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

  const save = async () => {
    if (!tenant || !selectedRouteId) return;
    setSaving(true); setError(''); setMessage('');
    try {
      const next = await api<FilterAssistantSettings>('/internal/v1/filter-assistant/settings', token, {
        method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, model_route_id: selectedRouteId }),
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
          {settings && <span className="status ok">{t('settings.configured')}</span>}
        </div>
        {loading ? <div className="empty" role="status">{t('common.loading')}</div> : loadError ? <div className="settings-empty" role="alert"><b>{t('settings.filterAssistantLoadFailed')}</b><span>{loadError}</span><button type="button" className="secondary" onClick={() => void load()}>{t('common.retry')}</button></div> : assistantOptions.length === 0 ? <div className="settings-empty"><b>{t('settings.noEnabledRoute')}</b><span>{t('settings.noEnabledRouteHint')}</span></div> : <form className="system-settings-form" onSubmit={(event) => { event.preventDefault(); void save(); }}>
          <div><ModelPicker label={t('settings.filterAssistantRoute')} popupLabel={t('settings.filterAssistantRoute')} value={selectedRouteIsVerified ? selectedRouteId : ''} onChange={setSelectedRouteId} disabled={saving} options={assistantOptions} describedBy="filter-assistant-route-hint" /><small id="filter-assistant-route-hint">{t('settings.assistantTextHint')}</small>{selectedRouteId && !selectedUnverifiedRoute && !selectedRouteHasAvailableCandidate && <small className="error-text" role="alert">{t('settings.assistantTextUnavailable')}</small>}</div>
          <button type="submit" disabled={saving || !selectedRouteId || !selectedRouteHasAvailableCandidate}>{saving ? t('common.loading') : t('common.save')}</button>
        </form>}
        {!loading && !loadError && selectedUnverifiedRoute && <p className="error-text" role="alert">{t('settings.assistantTextSelectedUnverified', { model: selectedUnverifiedRoute.label })}</p>}
        {!loading && !loadError && unverifiedRoutes.length > 0 && <p className="settings-status-note" role="status">{t('settings.assistantTextUnverified', { count: unverifiedRoutes.length })}</p>}
        {settings === null && !loadError && <p className="settings-status-note">{t('settings.filterAssistantNotConfigured')}</p>}
      </article>
    </>}
  </section>;
}
