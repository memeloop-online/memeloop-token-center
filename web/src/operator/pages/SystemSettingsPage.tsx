import { useEffect, useState } from 'react';
import { api } from '../../api';
import { useI18n } from '../../i18n';
import type { FilterAssistantSettings, ModelRouteView } from '../../types';
import { messageOf, queryForTenant } from '../scope/operatorShared';

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
    <form className="system-settings-access operator-credential" onSubmit={(event) => { event.preventDefault(); if (credentialInput.trim()) onConnect(credentialInput); }}>
      <label htmlFor="operator-access-credential">{t('settings.accessCredential')}<input id="operator-access-credential" autoComplete="new-password" type="password" value={credentialInput} onChange={(event) => onCredentialInput(event.target.value)} placeholder={t('operator.tokenPlaceholder')} /></label>
      <div className="button-row"><button type="submit" disabled={authenticating || !credentialInput.trim()}>{authenticating ? t('common.loading') : credential ? t('settings.replaceCredential') : t('common.connect')}</button>{credential && <button type="button" className="secondary" onClick={onClear}>{t('common.clearCredential')}</button>}</div>
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
  const [settings, setSettings] = useState<FilterAssistantSettings | null>();
  const [selectedRouteId, setSelectedRouteId] = useState('');
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');

  useEffect(() => {
    setError(''); setMessage(''); setSelectedRouteId('');
    if (!token || !tenant) { setRoutes([]); setSettings(undefined); return; }
    let cancelled = false;
    setLoading(true);
    void Promise.all([
      api<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(tenant)}`, token),
      api<FilterAssistantSettings | null>(`/internal/v1/filter-assistant/settings?tenant_external_id=${encodeURIComponent(tenant)}`, token),
    ]).then(([nextRoutes, nextSettings]) => {
      if (cancelled) return;
      const enabled = nextRoutes.filter((route) => route.enabled);
      setRoutes(enabled); setSettings(nextSettings); setSelectedRouteId(nextSettings?.model_route_id ?? '');
    }).catch((reason: unknown) => { if (!cancelled) setError(messageOf(reason, t('common.requestFailed'))); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, [tenant, token]);

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
        {loading ? <div className="empty">{t('common.loading')}</div> : routes.length === 0 ? <div className="settings-empty"><b>{t('settings.noEnabledRoute')}</b><span>{t('settings.noEnabledRouteHint')}</span></div> : <div className="system-settings-form">
          <label htmlFor="filter-assistant-route">{t('settings.filterAssistantRoute')}<select id="filter-assistant-route" value={selectedRouteId} onChange={(event) => setSelectedRouteId(event.target.value)}><option value="">{t('common.select')}</option>{routes.map((route) => <option key={route.id} value={route.id}>{route.public_model} · {route.upstream_model} · {route.protocol}</option>)}</select><small>{t('settings.filterAssistantRouteHint')}</small></label>
          <button type="button" disabled={saving || !selectedRouteId} onClick={() => void save()}>{saving ? t('common.loading') : t('common.save')}</button>
        </div>}
        {settings === null && <p className="settings-status-note">{t('settings.filterAssistantNotConfigured')}</p>}
      </article>
    </>}
  </section>;
}
