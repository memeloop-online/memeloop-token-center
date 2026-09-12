import { useId, useState } from 'react';
import { api } from '../api';
import { useI18n } from '../i18n';
import type { UpstreamAccount } from '../types';
import { isPrivateProxyUrl } from './upstreamConnectionPolicy';
export { connectionSchema, isPrivateProxyUrl } from './upstreamConnectionPolicy';
import './upstreamConnection.css';

export function ProxyInput({ value, onChange, disabled = false }: { value: string; onChange: (value: string) => void; disabled?: boolean }) {
  const { t } = useI18n();
  const id = useId();
  const invalid = Boolean(value && !isPrivateProxyUrl(value.trim()));
  return <div className="upstream-proxy-editor">
    <label>{t('connection.proxyUrl')} · {t('connection.required')}<input type="password" required aria-invalid={invalid} aria-describedby={`${id}-hint${invalid ? ` ${id}-error` : ''}`} autoComplete="new-password" spellCheck={false} disabled={disabled} value={value} onChange={(event) => onChange(event.target.value)} placeholder="socks5h://10.0.0.10:1080" /></label>
    <p id={`${id}-hint`}>{t('connection.proxyHint')}</p>
    {invalid && <p id={`${id}-error`} role="alert">{t('connection.proxyInvalid')}</p>}
  </div>;
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
  const canEditProxy = codex && account.can_update_transport_proxy === true;
  const proxyState = account.has_proxy === undefined ? 'connection.proxyUnknown' : account.has_proxy ? account.proxy_scheme ? 'connection.proxyConfigured' : 'connection.proxyNeedsUpdate' : 'connection.proxyMissing';
  const valid = isPrivateProxyUrl(proxy.trim());
  async function save() {
    if (!valid || busy || disabled || !canEditProxy) return;
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
      <div><dt>{t('connection.baseUrl')}</dt><dd><code>{typeof account.config.base_url === 'string' ? account.config.base_url : '—'}</code>{codex && <span className="pill">{t('connection.fixed')}</span>}</dd></div>
      <div><dt>{t('connection.proxy')}</dt><dd><span className={`status ${account.has_proxy && account.proxy_scheme ? 'ok' : 'pending'}`}>{t(account.has_proxy === undefined ? 'connection.proxyUnknown' : account.has_proxy ? proxyState : codex ? proxyState : 'connection.directEgress')}</span>{account.proxy_scheme && <code>{account.proxy_scheme}</code>}{account.has_proxy && account.proxy_scheme && <span>{t(account.proxy_remote_dns ? 'connection.remoteDns' : 'connection.localDns')}</span>}</dd></div>
      {account.proxy_fingerprint && <div><dt>{t('connection.proxyFingerprint')}</dt><dd><code>{account.proxy_fingerprint}</code></dd></div>}
    </dl>
    <p className="muted">{t('connection.endpointHint')}</p>
    {codex && !canEditProxy && <p>{t('connection.proxyAdminOnly')}</p>}
    {!codex && <p>{t('connection.genericProxyHint')}</p>}
    {canEditProxy && <><button type="button" className="secondary" disabled={disabled || busy} onClick={() => { setEditing(!editing); setProxy(''); setError(false); setSaved(false); }}>{t(editing ? 'common.cancel' : 'connection.editProxy')}</button>
      {editing && <form className="upstream-proxy-editor" onSubmit={(event) => { event.preventDefault(); void save(); }}>
        <ProxyInput value={proxy} onChange={setProxy} disabled={busy} />
        <button type="submit" disabled={disabled || busy || !valid}>{t(busy ? 'common.loading' : 'connection.saveProxy')}</button>
      </form>}
    </>}
    {error && <p className="notice error" role="alert">{t('connection.saveFailed')}</p>}
    {saved && <p role="status">{t('connection.saved')}</p>}
  </section>;
}
