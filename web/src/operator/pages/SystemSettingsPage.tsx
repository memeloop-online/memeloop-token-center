import { useEffect, useState } from 'react';
import { api } from '../../api';
import { useI18n } from '../../i18n';
import type { FilterAssistantSettings, ModelRouteView } from '../../types';
import { messageOf, queryForTenant } from '../scope/operatorShared';

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

  const selected = routes.find((route) => route.id === selectedRouteId);
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

  return <section className="system-settings"><article className="panel"><div className="panel-title"><div><h2>{t('settings.title')}</h2><p className="muted">{t('settings.description')}</p></div></div>
    {!tenant ? <div className="empty">{t('settings.selectTenant')}</div> : <>
      {error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      {loading ? <div className="empty">{t('common.loading')}</div> : routes.length === 0 ? <div className="empty">{t('settings.noEnabledRoute')}</div> : <div className="system-settings-form"><label>{t('settings.filterAssistantRoute')}<select value={selectedRouteId} onChange={(event) => setSelectedRouteId(event.target.value)}><option value="">{t('common.select')}</option>{routes.map((route) => <option key={route.id} value={route.id}>{route.public_model} · {route.upstream_model} · {route.protocol}</option>)}</select></label>
        {selected && <p className="muted">{t('settings.filterAssistantRouteHint')} <code>{selected.id}</code></p>}
        <p className="muted">{t('settings.filterAssistantSecretFree')}</p>
        <button type="button" disabled={saving || !selectedRouteId} onClick={() => void save()}>{saving ? t('common.loading') : t('common.save')}</button>
      </div>}
      {settings === null && <div className="notice warning" role="status">{t('settings.filterAssistantNotConfigured')}</div>}
    </>}
  </article></section>;
}
