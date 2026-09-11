import { useState } from 'react';
import type { RJSFSchema } from '@rjsf/utils';
import { api } from '../api';
import { useI18n } from '../i18n';
import type { UpstreamAccount } from '../types';
import './upstreamConnection.css';

export function isPrivateProxyUrl(value: string) {
  try {
    const url = new URL(value);
    if (url.protocol !== 'socks5h:' || !url.port || url.search || url.hash || (url.pathname && url.pathname !== '/')) return false;
    const host = url.hostname;
    if (host.startsWith('[')) return /^\[f[cd][0-9a-f]{2}:/i.test(host);
    const octets = host.split('.').map(Number);
    return octets.length === 4 && octets.every((part) => Number.isInteger(part) && part >= 0 && part <= 255)
      && (octets[0] === 10 || (octets[0] === 172 && octets[1] >= 16 && octets[1] <= 31) || (octets[0] === 192 && octets[1] === 168));
  } catch { return false; }
}

export function ProxyInput({ value, onChange, disabled = false }: { value: string; onChange: (value: string) => void; disabled?: boolean }) {
  const { t } = useI18n();
  return <div className="upstream-proxy-editor">
    <label>{t('connection.proxyUrl')}<input type="password" autoComplete="new-password" spellCheck={false} disabled={disabled} value={value} onChange={(event) => onChange(event.target.value)} placeholder="socks5h://10.0.0.10:1080" /></label>
    <p>{t('connection.proxyHint')}</p>
    {value && !isPrivateProxyUrl(value.trim()) && <p role="alert">{t('connection.proxyInvalid')}</p>}
  </div>;
}

/** Preserve provider validation while making endpoint semantics explicit. */
export function connectionSchema(schema: RJSFSchema, endpointHint: string): RJSFSchema {
  const result = structuredClone(schema);
  const base = result.properties?.base_url;
  if (base && typeof base === 'object') {
    base.title = 'Base URL';
    base.description = endpointHint;
    if (base.const !== undefined) base.readOnly = true;
  }
  return result;
}

export function UpstreamConnection({ account, token, tenant, disabled, onChanged }: {
  account: UpstreamAccount; token: string; tenant: string; disabled: boolean; onChanged: () => Promise<void>;
}) {
  const { t } = useI18n();
  const [editing, setEditing] = useState(false);
  const [proxy, setProxy] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(false);
  const [saved, setSaved] = useState(false);
  const codex = account.driver === 'openai-codex' && account.auth_kind === 'oauth';
  const proxyState = account.has_proxy === undefined ? 'connection.proxyUnknown' : account.has_proxy ? 'connection.proxyConfigured' : 'connection.proxyMissing';
  const valid = isPrivateProxyUrl(proxy.trim());
  async function save() {
    if (!valid || busy || disabled) return;
    setBusy(true); setError(false); setSaved(false);
    try {
      await api(`/internal/v1/upstreams/${encodeURIComponent(account.id)}/transport-proxy`, token, {
        method: 'PUT', headers: { 'Idempotency-Key': crypto.randomUUID() },
        body: JSON.stringify({ tenant_external_id: tenant, proxy_url: proxy.trim(), expected_updated_at: account.updated_at, expected_credential_generation: account.credential_generation }),
      });
      setProxy(''); setEditing(false); setSaved(true);
      await onChanged();
    } catch { setError(true); }
    finally { setBusy(false); }
  }
  return <section className="upstream-connection" aria-label={t('connection.title')}>
    <h3>{t('connection.title')}</h3>
    <dl>
      <div><dt>Base URL</dt><dd><code>{typeof account.config.base_url === 'string' ? account.config.base_url : '—'}</code>{codex && <span className="pill">{t('connection.fixed')}</span>}</dd></div>
      <div><dt>{t('connection.proxy')}</dt><dd><span className={`status ${account.has_proxy ? 'ok' : 'pending'}`}>{t(codex ? proxyState : 'connection.proxyNotManaged')}</span>{account.proxy_scheme && <code>{account.proxy_scheme}</code>}{account.has_proxy && <span>{t(account.proxy_remote_dns ? 'connection.remoteDns' : 'connection.localDns')}</span>}</dd></div>
    </dl>
    <p className="muted">{t('connection.endpointHint')}</p>
    {codex && <><button type="button" className="secondary" disabled={disabled || busy} onClick={() => { setEditing(!editing); setProxy(''); setError(false); setSaved(false); }}>{t(editing ? 'common.cancel' : 'connection.editProxy')}</button>
      {editing && <form className="upstream-proxy-editor" onSubmit={(event) => { event.preventDefault(); void save(); }}>
        <ProxyInput value={proxy} onChange={setProxy} disabled={busy} />
        <button type="submit" disabled={disabled || busy || !valid}>{t(busy ? 'common.loading' : 'connection.saveProxy')}</button>
      </form>}
    </>}
    {error && <p className="notice error" role="alert">{t('connection.saveFailed')}</p>}
    {saved && <p role="status">{t('connection.saved')}</p>}
  </section>;
}
