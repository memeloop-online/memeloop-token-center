import { lazy, Suspense, useState } from 'react';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { PluginManifest } from '../types';

const PluginConfigurationForm = lazy(() => import('./PluginConfigurationForm').then((module) => ({ default: module.PluginConfigurationForm })));

function PluginConfigurationEditor({ token, tenant, writeTenant, plugin }: { token: string; tenant: string; writeTenant: string; plugin: PluginManifest }) {
  const { t } = useI18n();
  const [opened, setOpened] = useState(false);
  const contribution = plugin.contributions.configuration;
  if (!contribution) return null;
  return <details className="inline-editor form-panel" onToggle={(event) => { if (event.currentTarget.open) setOpened(true); }}>
    <summary>{t('plugins.editConfiguration')}</summary>
    {opened && <Suspense fallback={<div role="status">{t('common.loading')}</div>}><PluginConfigurationForm token={token} tenant={tenant} writeTenant={writeTenant} plugin={plugin} /></Suspense>}
  </details>;
}

/** `tenant` scopes reads; plugin configuration writes use `writeTenant`. */
export function Plugins({ token, tenant, writeTenant = tenant, values }: { token: string; tenant: string; writeTenant?: string; values: PluginManifest[] }) {
  const { locale, t } = useI18n();
  return <article className="panel">
    <div className="panel-title"><div><h2>{t('plugins.title')}</h2><p className="muted">{t('plugins.configurationDescription')}</p></div><span>{t('plugins.runtime')}</span></div>
    <div className="account-list">
      {values.length === 0 && <div className="empty">{t('plugins.empty')}</div>}
      {values.map((plugin) => <div className="managed-resource" key={plugin.id}>
        <div className="managed-resource-header"><div><b>{plugin.id}</b><span>v{plugin.version} · WIT {plugin.wit_version} · {t('plugins.providerCount', { count: formatNumber((plugin.contributions.providers ?? []).length, locale) })}</span></div><div className="account-meta">{plugin.contributions.traffic_policy && <span className="pill">{t('plugins.trafficPolicy')}</span>}{plugin.contributions.request_rewrite && <span className="pill">{t('plugins.requestRewrite')}</span>}{!plugin.contributions.traffic_policy && !plugin.contributions.request_rewrite && <span className="pill">{t('plugins.provider')}</span>}</div></div>
        <PluginConfigurationEditor key={`${token}\0${tenant}\0${writeTenant}\0${plugin.id}`} token={token} tenant={tenant} writeTenant={writeTenant} plugin={plugin} />
      </div>)}
    </div>
  </article>;
}
