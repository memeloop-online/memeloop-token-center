import { useEffect, useState } from 'react';
import { Button } from '@fluentui/react-components';
import { api } from '../api';
import { useI18n } from '../i18n';

type Settings = { enabled: boolean; billing_key_id: string | null; interval_seconds: number; model: string | null; prompt: string };
type Key = { key_id: string; alias: string; status: string };

export function HealthProbeSettings({ token, tenant, accountId, disabled }: { token: string; tenant: string; accountId: string; disabled: boolean }) {
  const { locale } = useI18n();
  const zh = locale.startsWith('zh');
  const [value, setValue] = useState<Settings>();
  const [keys, setKeys] = useState<Key[]>([]);
  const [error, setError] = useState('');
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const path = `/internal/v1/upstreams/${encodeURIComponent(accountId)}/health-probe-settings?tenant_external_id=${encodeURIComponent(tenant)}`;
  useEffect(() => {
    const controller = new AbortController();
    setValue(undefined); setError(''); setSaved(false);
    Promise.all([
      api<Settings>(path, token, { signal: controller.signal }),
      api<Key[]>(`/internal/v1/keys?tenant_external_id=${encodeURIComponent(tenant)}&limit=500`, token, { signal: controller.signal }),
    ]).then(([settings, credentials]) => {
      if (!settings || typeof settings.enabled !== 'boolean' || typeof settings.interval_seconds !== 'number' || typeof settings.prompt !== 'string' || !Array.isArray(credentials)) throw new Error(zh ? '探测设置格式无效' : 'Invalid probe settings');
      setValue(settings); setKeys(credentials.filter(key => key.status === 'active'));
    })
      .catch(reason => { if (!controller.signal.aborted) setError(String(reason)); });
    return () => controller.abort();
  }, [path, tenant, token, zh]);
  async function save() {
    if (!value) return;
    setSaving(true); setError(''); setSaved(false);
    try { setValue(await api<Settings>(path, token, { method: 'PUT', body: JSON.stringify(value) })); setSaved(true); }
    catch (reason) { setError(String(reason)); } finally { setSaving(false); }
  }
  return <section className="settings-card" aria-label={zh ? '上游健康探测' : 'Upstream health probe'}>
    <h3>{zh ? '上游健康探测' : 'Upstream health probe'}</h3>
    <p>{zh ? '定时发送短请求，使用独立凭据计费。间隔至少为一小时；手动检查不受此间隔限制。' : 'Send a short scheduled request with a separate billing credential. The minimum interval is one hour; manual checks are independent.'}</p>
    {error && <p role="alert">{error}</p>}{saved && <p role="status">{zh ? '已保存' : 'Saved'}</p>}
    {value && <fieldset disabled={disabled || saving}>
      <label><input type="checkbox" checked={value.enabled} onChange={e => setValue({ ...value, enabled: e.target.checked })} /> {zh ? '启用定时探测' : 'Enable scheduled probes'}</label>
      <label>{zh ? '独立计费凭据' : 'Separate billing credential'}<select value={value.billing_key_id ?? ''} onChange={e => setValue({ ...value, billing_key_id: e.target.value || null })}><option value="">{zh ? '请选择' : 'Select'}</option>{keys.map(key => <option key={key.key_id} value={key.key_id}>{key.alias}</option>)}</select></label>
      <label>{zh ? '探测间隔（小时）' : 'Interval (hours)'}<input type="number" min="1" max="720" step="1" value={value.interval_seconds / 3600} onChange={e => setValue({ ...value, interval_seconds: Number(e.target.value) * 3600 })} /></label>
      <label>{zh ? '模型（留空自动选择低成本模型）' : 'Model (leave empty to choose a low-cost model)'}<input value={value.model ?? ''} maxLength={200} onChange={e => setValue({ ...value, model: e.target.value || null })} /></label>
      <label>{zh ? '探测提示词' : 'Probe prompt'}<textarea value={value.prompt} maxLength={1024} onChange={e => setValue({ ...value, prompt: e.target.value })} /></label>
      <Button appearance="primary" type="button" style={{ minHeight: 44, maxWidth: "100%" }} onClick={() => void save()}>{zh ? '保存探测设置' : 'Save probe settings'}</Button>
    </fieldset>}
  </section>;
}
