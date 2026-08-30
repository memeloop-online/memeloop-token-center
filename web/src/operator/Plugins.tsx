import RjsfForm from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { localizeSchema, useI18n } from '../i18n';
import { schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator as validator } from '../safeValidator';
import type { PluginConfiguration, PluginManifest } from '../types';

function queryForTenant(tenant: string) {
  const query = new URLSearchParams();
  if (tenant) query.set('tenant_external_id', tenant);
  const encoded = query.toString();
  return encoded ? `?${encoded}` : '';
}

export function Plugins({ token, tenant, values }: { token: string; tenant: string; values: PluginManifest[] }) {
  const { locale, t } = useI18n();
  const [configurations, setConfigurations] = useState<Record<string, PluginConfiguration>>({});
  const [saving, setSaving] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const loadSequence = useRef(0);
  const scope = useRef({ token, tenant });
  scope.current = { token, tenant };

  const load = async () => {
    const sequence = ++loadSequence.current;
    const loadToken = token;
    const loadTenant = tenant;
    const configurable = values.filter((plugin) => plugin.contributions.configuration);
    if (!loadToken || configurable.length === 0) { setConfigurations({}); return; }
    try {
      const loaded = await Promise.all(configurable.map(async (plugin) => [
        plugin.id,
        await api<PluginConfiguration>(`/internal/v1/plugins/${plugin.id}/configuration${queryForTenant(loadTenant)}`, loadToken),
      ] as const));
      if (sequence !== loadSequence.current || scope.current.token !== loadToken || scope.current.tenant !== loadTenant) return;
      setConfigurations(Object.fromEntries(loaded));
      setError('');
    } catch (reason) {
      if (sequence !== loadSequence.current || scope.current.token !== loadToken || scope.current.tenant !== loadTenant) return;
      setError(reason instanceof Error ? reason.message : t('common.requestFailed'));
    }
  };

  useEffect(() => {
    loadSequence.current += 1;
    setConfigurations({}); setSaving(''); setMessage(''); setError('');
    void load();
  }, [token, tenant, values]);

  return <article className="panel">
    <div className="panel-title"><div><h2>{t('plugins.title')}</h2><p className="muted">{t('plugins.configurationDescription')}</p></div><span>{t('plugins.runtime')}</span></div>
    {error && <div className="notice error" role="alert">{error}</div>}
    {message && <div className="notice success" role="status">{message}</div>}
    <div className="account-list">
      {values.length === 0 && <div className="empty">{t('plugins.empty')}</div>}
      {values.map((plugin) => {
        const contribution = plugin.contributions.configuration;
        const configuration = configurations[plugin.id];
        return <div className="managed-resource" key={plugin.id}>
          <div className="managed-resource-header"><div><b>{plugin.id}</b><span>v{plugin.version} · WIT {plugin.wit_version} · {t('plugins.providerCount', { count: formatNumber((plugin.contributions.providers ?? []).length, locale) })}</span></div><div className="account-meta">{plugin.contributions.traffic_policy && <span className="pill">{t('plugins.trafficPolicy')}</span>}{plugin.contributions.request_rewrite && <span className="pill">{t('plugins.requestRewrite')}</span>}{!plugin.contributions.traffic_policy && !plugin.contributions.request_rewrite && <span className="pill">{t('plugins.provider')}</span>}</div></div>
          {contribution && configuration && <div className="inline-editor form-panel">
            <p className="muted">{t('plugins.configurationScope', { source: t(`plugins.source.${configuration.source}`), version: formatNumber(configuration.scope_version, locale) })}</p>
            <RjsfForm
              key={`${plugin.id}-${tenant}-${configuration.scope_version}-${locale}`}
              schema={localizeSchema(contribution.schema as RJSFSchema, locale)}
              formData={configuration.value}
              validator={validator}
              templates={schemaFormTemplates}
              noHtml5Validate
              onError={() => { /* RJSF renders bounded validation errors inline. */ }}
              onSubmit={async ({ formData }) => {
                if (!tenant) return;
                const saveToken = token; const saveTenant = tenant;
                setSaving(plugin.id); setMessage(''); setError('');
                try {
                  await api<PluginConfiguration>(`/internal/v1/plugins/${plugin.id}/configuration`, saveToken, {
                    method: 'PUT',
                    headers: { 'Idempotency-Key': crypto.randomUUID() },
                    body: JSON.stringify({ tenant_external_id: saveTenant || null, expected_version: configuration.scope_version, value: formData }),
                  });
                  if (scope.current.token !== saveToken || scope.current.tenant !== saveTenant) return;
                  setMessage(t('plugins.configurationSaved', { plugin: plugin.id }));
                  await load();
                } catch (reason) {
                  if (scope.current.token !== saveToken || scope.current.tenant !== saveTenant) return;
                  setError(reason instanceof Error ? reason.message : t('common.requestFailed'));
                } finally { if (scope.current.token === saveToken && scope.current.tenant === saveTenant) setSaving(''); }
              }}
            ><button type="submit" disabled={!tenant || saving === plugin.id}>{saving === plugin.id ? t('common.loading') : t('common.save')}</button></RjsfForm>
          </div>}
        </div>;
      })}
    </div>
  </article>;
}
