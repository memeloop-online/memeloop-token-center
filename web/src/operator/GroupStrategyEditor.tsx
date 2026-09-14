import RjsfForm from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useRef, useState } from 'react';
import { api, ApiError } from '../api';
import { localizeSchema, useI18n } from '../i18n';
import { schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator } from '../safeValidator';
import type { GroupView } from '../types';

export interface GroupRoutingStrategyOption {
  id: string;
  version: string;
  schema: RJSFSchema;
  default: Record<string, unknown>;
}

/** Parent keys by tenant, token, kind and group. Refresh never discards edits. */
export function GroupStrategyEditor({ kind, token, tenant, group, onChanged }: {
  kind: 'provider' | 'route'; token: string; tenant: string; group: GroupView; onChanged: () => Promise<void>;
}) {
  const { locale, t } = useI18n();
  const [catalog, setCatalog] = useState<GroupRoutingStrategyOption[]>([]);
  const [loading, setLoading] = useState(true);
  const [catalogError, setCatalogError] = useState('');
  const [reload, setReload] = useState(0);
  const [pluginId, setPluginId] = useState(group.routing_strategy?.plugin_id ?? '');
  const [config, setConfig] = useState(group.routing_strategy?.config ?? {});
  const [priority, setPriority] = useState(String(group.routing_priority ?? 0));
  const [snapshot, setSnapshot] = useState(group);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [needsRefresh, setNeedsRefresh] = useState(false);
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setCatalogError('');
    void api<GroupRoutingStrategyOption[]>('/internal/v1/plugins/group-routing-strategies', token,
      { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]) })
      .then(value => { if (!controller.signal.aborted) setCatalog(value.filter(item => item.version === 'group-routing-v1')); })
      .catch(reason => { if (!controller.signal.aborted) setCatalogError(reason instanceof Error ? reason.message : t('common.requestFailed')); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [token, reload]);
  const option = catalog.find(value => value.id === pluginId);
  const validPriority = /^-?\d+$/.test(priority) && Number(priority) >= -2147483648 && Number(priority) <= 2147483647;
  async function refresh() {
    const query = new URLSearchParams({ tenant_external_id: tenant });
    const groups = await api<GroupView[]>(`/internal/v1/${kind}-groups?${query}`, token, { signal: AbortSignal.timeout(10_000) });
    const current = groups.find(value => value.id === group.id);
    if (!current) throw new Error(t('groups.strategyMissingGroup'));
    if (!mounted.current) return;
    setSnapshot(current); setNeedsRefresh(false);
    await onChanged();
  }
  async function save(formData: Record<string, unknown>) {
    if (saving || !validPriority || needsRefresh || (pluginId && !option)) return;
    setSaving(true); setError(''); setMessage('');
    try {
      const saved = await api<GroupView>(`/internal/v1/${kind}-groups/${encodeURIComponent(group.id)}/routing-strategy`, token, {
        method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, expected_updated_at: snapshot.updated_at,
          expected_strategy_version: snapshot.strategy_version ?? 0, routing_priority: Number(priority),
          routing_strategy: pluginId ? { plugin_id: pluginId, config: formData } : null }),
      });
      if (!mounted.current) return;
      setSnapshot(saved); setMessage(t('groups.strategySaved'));
      await onChanged();
    } catch (reason) {
      if (!mounted.current) return;
      if (reason instanceof ApiError && reason.status === 409) {
        setNeedsRefresh(true);
        try { await refresh(); if (mounted.current) setError(t('groups.strategyConflict')); }
        catch { if (mounted.current) setError(t('groups.strategyRefreshFailed')); }
      } else setError(reason instanceof Error ? reason.message : t('common.requestFailed'));
    } finally { if (mounted.current) setSaving(false); }
  }
  return <section className="form-panel group-strategy-editor" aria-label={t('groups.strategy')}>
    <h3>{t('groups.strategy')}</h3>
    <p className="muted">{t('groups.strategyHelp')}</p>
    {loading && <p role="status">{t('common.loading')}</p>}
    {catalogError && <div role="alert">{catalogError}<button type="button" onClick={() => setReload(value => value + 1)}>{t('common.retry')}</button></div>}
    {error && <div className="notice error" role="alert">{error}</div>}
    {message && <div className="notice success" role="status">{message}</div>}
    {needsRefresh && <button type="button" disabled={saving} onClick={async () => {
      setSaving(true);
      try { await refresh(); if (mounted.current) setError(t('groups.strategyConflict')); }
      catch { if (mounted.current) setError(t('groups.strategyRefreshFailed')); }
      finally { if (mounted.current) setSaving(false); }
    }}>{t('common.retry')}</button>}
    <label>{t('groups.strategy')}<select disabled={saving} value={pluginId} onChange={event => { setPluginId(event.target.value); setConfig(catalog.find(value => value.id === event.target.value)?.default ?? {}); setMessage(''); }}>
      <option value="">{t('groups.strategyNative')}</option>
      {pluginId && !option && <option value={pluginId} disabled>{pluginId} — {t('groups.strategyUnavailable')}</option>}
      {catalog.map(value => <option key={value.id} value={value.id}>{value.id}</option>)}
    </select></label>
    {pluginId && !option && !loading && <p role="alert">{t('groups.strategyUnavailable')}</p>}
    <label>{t('groups.strategyPriority')}<input type="number" step="1" min={-2147483648} max={2147483647} value={priority} disabled={saving} onChange={event => setPriority(event.target.value)} /></label>
    <p className="muted">{t('groups.strategyPriorityHelp')}</p>
    <RjsfForm key={pluginId} idPrefix={`group-strategy-${group.id}`} schema={localizeSchema(option?.schema ?? { type: 'object', properties: {} }, locale)}
      formData={config} validator={safeValidator} templates={schemaFormTemplates} disabled={saving}
      noHtml5Validate onChange={({ formData }) => setConfig(formData ?? {})} onSubmit={({ formData }) => void save(formData ?? {})}>
      <button type="submit" disabled={saving || !token || !tenant || !validPriority || needsRefresh || Boolean(pluginId && (!option || loading || catalogError))}>{t('groups.strategySave')}</button>
    </RjsfForm>
  </section>;
}
