import { lazy, Suspense, useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { Button, Disclosure } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { PluginConfiguration, PluginManifest } from '../types';
import { queryForTenant } from './scope/operatorShared';

const PluginConfigurationForm = lazy(() => import('./PluginConfigurationForm').then((module) => ({ default: module.PluginConfigurationForm })));

function PluginConfigurationEditor({ token, tenant, writeTenant, plugin }: { token: string; tenant: string; writeTenant: string; plugin: PluginManifest }) {
  const { locale, t } = useI18n();
  const [opened, setOpened] = useState(false);
  const [configuration, setConfiguration] = useState<PluginConfiguration>();
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const pending = useRef<AbortController | undefined>(undefined);
  const mounted = useRef(true);
  const contribution = plugin.contributions.configuration;
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; pending.current?.abort(); };
  }, []);

  async function load(force = false) {
    if (!token || pending.current || (!force && configuration)) return;
    const controller = new AbortController();
    pending.current = controller;
    setLoading(true); setError('');
    try {
      const value = await api<PluginConfiguration>(
        `/internal/v1/plugins/${encodeURIComponent(plugin.id)}/configuration${queryForTenant(tenant)}`,
        token,
        { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]) },
      );
      if (mounted.current && !controller.signal.aborted) setConfiguration(value);
    } catch (reason) {
      if (mounted.current && !controller.signal.aborted) setError(reason instanceof Error ? reason.message : t('common.requestFailed'));
    } finally {
      if (pending.current === controller) pending.current = undefined;
      if (mounted.current) setLoading(false);
    }
  }

  async function save(formData: unknown) {
    if (!configuration || !writeTenant || saving) return;
    setSaving(true); setMessage(''); setError('');
    try {
      const saved = await api<PluginConfiguration>(`/internal/v1/plugins/${encodeURIComponent(plugin.id)}/configuration`, token, {
        method: 'PUT',
        headers: { 'Idempotency-Key': crypto.randomUUID() },
        body: JSON.stringify({ tenant_external_id: writeTenant, expected_version: configuration.scope_version, value: formData }),
      });
      if (!mounted.current) return;
      // A write may target an explicit tenant while the selected read scope
      // is global. Do not relabel the saved tenant value as global.
      if (writeTenant === tenant) setConfiguration(saved);
      else await load(true);
      if (mounted.current) setMessage(t('plugins.configurationSaved', { plugin: plugin.id }));
    } catch (reason) {
      if (mounted.current) setError(reason instanceof Error ? reason.message : t('common.requestFailed'));
    } finally { if (mounted.current) setSaving(false); }
  }

  if (!contribution) return null;
  return <div className="plugin-configuration-editor form-panel"><Disclosure title={t('plugins.editConfiguration')} open={opened} onOpenChange={(open) => { setOpened(open); if (open) void load(); }}>
    {opened && <>
      {loading && <div role="status">{t('common.loading')}</div>}
      {error && <div className="notice error" role="alert">{error}{!configuration && <button type="button" onClick={() => void load()}>{t('common.retry')}</button>}</div>}
      {message && <div className="notice success" role="status">{message}</div>}
      {configuration && <p className="muted">{t('plugins.configurationScope', { source: t(`plugins.source.${configuration.source}`), version: formatNumber(configuration.scope_version, locale) })}</p>}
      <Suspense fallback={loading ? null : <div role="status">{t('common.loading')}</div>}><PluginConfigurationForm plugin={plugin} configuration={configuration} saving={saving} writeTenant={writeTenant} onSubmit={save} /></Suspense>
    </>}
  </Disclosure></div>;
}

/** `tenant` scopes reads; plugin configuration writes use `writeTenant`. */
export function Plugins({ token, tenant, writeTenant = tenant, values, onRefresh, refreshError }: { token: string; tenant: string; writeTenant?: string; values: PluginManifest[]; onRefresh?: () => Promise<void>; refreshError?: string }) {
  const { locale, t } = useI18n();
  return <article className="panel">
    <div className="panel-title"><div><h2>{t('plugins.title')}</h2><p className="muted">{t('plugins.configurationDescription')}</p></div>{onRefresh && <Button appearance="secondary" onClick={() => void onRefresh()}>{t('plugins.refreshCatalog')}</Button>}</div>
    {refreshError && <div className="notice error" role="alert">{refreshError}</div>}
    <div className="account-list">
      {values.length === 0 && <div className="empty">{t('plugins.empty')}</div>}
      {values.map((plugin) => <div className="managed-resource" key={plugin.id}>
        <div className="managed-resource-header"><div><b>{plugin.id}</b><span>v{plugin.version} · WIT {plugin.wit_version} · {t('plugins.providerCount', { count: formatNumber((plugin.contributions.providers ?? []).length, locale) })}</span></div><div className="account-meta">{plugin.contributions.traffic_policy && <span className="pill">{t('plugins.trafficPolicy')}</span>}{plugin.contributions.request_rewrite && <span className="pill">{t('plugins.requestRewrite')}</span>}{!plugin.contributions.traffic_policy && !plugin.contributions.request_rewrite && <span className="pill">{t('plugins.provider')}</span>}</div></div>
        <PluginConfigurationEditor key={`${token}\0${tenant}\0${writeTenant}\0${plugin.id}`} token={token} tenant={tenant} writeTenant={writeTenant} plugin={plugin} />
      </div>)}
    </div>
  </article>;
}
