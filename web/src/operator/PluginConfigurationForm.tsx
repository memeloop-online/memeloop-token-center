import RjsfForm from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { localizeSchema, useI18n } from '../i18n';
import { schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator as validator } from '../safeValidator';
import type { PluginConfiguration, PluginManifest } from '../types';
import { queryForTenant } from './scope/operatorShared';

/** Heavy schema form is mounted only after its disclosure is opened. */
export function PluginConfigurationForm({ token, tenant, writeTenant, plugin }: { token: string; tenant: string; writeTenant: string; plugin: PluginManifest }) {
  const { locale, t } = useI18n();
  const [configuration, setConfiguration] = useState<PluginConfiguration>();
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const pending = useRef<AbortController | undefined>(undefined);
  const mounted = useRef(true);
  const contribution = plugin.contributions.configuration!;

  useEffect(() => {
    mounted.current = true;
    void load();
    return () => {
      mounted.current = false;
      const request = pending.current;
      pending.current = undefined;
      request?.abort();
    };
    // The parent keys this form by credential/read/write scope and plugin.
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
      if (pending.current === controller) {
        pending.current = undefined;
        if (mounted.current) setLoading(false);
      }
    }
  }

  return <>
    {loading && <div role="status">{t('common.loading')}</div>}
    {error && <div className="notice error" role="alert">{error}{!configuration && <button type="button" onClick={() => void load()}>{t('common.retry')}</button>}</div>}
    {message && <div className="notice success" role="status">{message}</div>}
    {configuration && <>
      <p className="muted">{t('plugins.configurationScope', { source: t(`plugins.source.${configuration.source}`), version: formatNumber(configuration.scope_version, locale) })}</p>
      <RjsfForm
        key={`${plugin.id}-${configuration.scope_version}-${locale}`}
        schema={localizeSchema(contribution.schema as RJSFSchema, locale)}
        formData={configuration.value}
        validator={validator}
        templates={schemaFormTemplates}
        noHtml5Validate
        onError={() => { /* RJSF renders bounded validation errors inline. */ }}
        onSubmit={async ({ formData }) => {
          if (!writeTenant || saving) return;
          setSaving(true); setMessage(''); setError('');
          try {
            const saved = await api<PluginConfiguration>(`/internal/v1/plugins/${encodeURIComponent(plugin.id)}/configuration`, token, {
              method: 'PUT',
              headers: { 'Idempotency-Key': crypto.randomUUID() },
              body: JSON.stringify({ tenant_external_id: writeTenant, expected_version: configuration.scope_version, value: formData }),
            });
            if (!mounted.current) return;
            // A write may target an explicit tenant while the selected read
            // scope is global. Do not relabel the saved tenant value as global.
            if (writeTenant === tenant) setConfiguration(saved);
            else await load(true);
            if (!mounted.current) return;
            setMessage(t('plugins.configurationSaved', { plugin: plugin.id }));
          } catch (reason) {
            if (mounted.current) setError(reason instanceof Error ? reason.message : t('common.requestFailed'));
          } finally { if (mounted.current) setSaving(false); }
        }}
      ><button type="submit" disabled={!writeTenant || saving}>{saving ? t('common.loading') : t('common.save')}</button></RjsfForm>
    </>}
  </>;
}
