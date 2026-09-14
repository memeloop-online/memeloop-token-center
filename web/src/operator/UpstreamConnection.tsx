import { useId, useLayoutEffect, useRef, useState } from 'react';
import { api } from '../api';
import { Button, DetailTooltip } from '../design-system';
import { useI18n } from '../i18n';
import type { UpstreamAccount } from '../types';
import { isPrivateProxyUrl, isGenericPrivateProxyUrl } from './upstreamConnectionPolicy';
export { connectionSchema, isPrivateProxyUrl } from './upstreamConnectionPolicy';
import './upstreamConnection.css';
import { SecretInput } from '../SecretInput';
import { CopyButton } from '../CopyButton';
import { providerConnectionCopy } from './providerConnectionCopy';

interface ProxyConnection {
  account_id: string;
  proxy_url: string | null;
  updated_at: number;
  credential_generation: number;
}

function ProxyValue({ value }: { value: string }) {
  const { locale, t } = useI18n();
  const copy = providerConnectionCopy(locale);
  const input = useRef<HTMLInputElement>(null);
  const [visible, setVisible] = useState(false);
  useLayoutEffect(() => { if (input.current) input.current.value = value; }, [value]);
  useLayoutEffect(() => {
    const hide = () => { if (input.current) input.current.type = 'password'; setVisible(false); };
    window.addEventListener('blur', hide);
    document.addEventListener('visibilitychange', hide);
    return () => { window.removeEventListener('blur', hide); document.removeEventListener('visibilitychange', hide); };
  }, []);
  return <div className="provider-proxy-value" onBlur={event => { if (!event.currentTarget.contains(event.relatedTarget)) setVisible(false); }} onKeyDown={event => { if (event.key === 'Escape' && visible) { setVisible(false); event.stopPropagation(); } }}>
    <input ref={input} type={visible ? 'text' : 'password'} readOnly aria-label={t('connection.proxyUrl')} autoComplete="off" spellCheck={false} />
    <Button type="button" appearance="secondary" aria-pressed={visible} onClick={() => setVisible(current => !current)}>{visible ? copy.hideProxy : copy.viewProxy}</Button>
    <CopyButton value={value} label={copy.copyProxy} />
  </div>;
}

export function ProxyInput({ value, onChange, disabled = false, generic = false }: { value: string; onChange: (value: string) => void; disabled?: boolean; generic?: boolean }) {
  const { t } = useI18n();
  const id = useId();
  const invalid = Boolean(value && !(generic ? isGenericPrivateProxyUrl(value.trim()) : isPrivateProxyUrl(value.trim())));
  return <div className="upstream-proxy-editor">
    <label htmlFor={id}>{t('connection.proxyUrl')} · {t('connection.required')}</label><SecretInput id={id} label={t('connection.proxyUrl')} required aria-invalid={invalid} aria-describedby={`${id}-hint${invalid ? ` ${id}-error` : ''}`} autoComplete="new-password" disabled={disabled} value={value} onChange={(event) => onChange(event.target.value)} placeholder="socks5h://10.0.0.10:1080" />
    <p id={`${id}-hint`}>{t(generic ? 'connection.genericProxyHint' : 'connection.proxyHint')}</p>
    {invalid && <p id={`${id}-error`} role="alert">{t(generic ? 'connection.genericProxyHint' : 'connection.proxyInvalid')}</p>}
  </div>;
}

export function UpstreamConnection({ account, token, tenant, disabled, onChanged, onEditingChange, onSaved, embedded = false }: {
  account: UpstreamAccount; token: string; tenant: string; disabled: boolean; onChanged: () => Promise<void>;
  onEditingChange?: (editing: boolean) => void;
  onSaved?: (account: UpstreamAccount) => void;
  embedded?: boolean;
}) {
  const { t, locale } = useI18n();
  const copy = providerConnectionCopy(locale);
  const [editing, setEditing] = useState(false);
  const [proxy, setProxy] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(false);
  const [saved, setSaved] = useState(false);
  const [connection, setConnection] = useState<ProxyConnection>();
  const [readError, setReadError] = useState(false);
  const [requested, setRequested] = useState(embedded);
  const scope = `${token}\0${tenant}\0${account.id}\0${account.credential_generation}`;
  const owner = useRef(scope);
  owner.current = scope;
  const saving = useRef(false);
  const readEpoch = useRef(0);
  useLayoutEffect(() => {
    setConnection(undefined); setReadError(false);
    setProxy(''); setEditing(false); setError(false); setSaved(false);
    if (!requested || disabled) return;
    const controller = new AbortController();
    const epoch = ++readEpoch.current;
    const query = new URLSearchParams({ tenant_external_id: tenant });
    void api<ProxyConnection>(`/internal/v1/upstreams/${encodeURIComponent(account.id)}/transport-proxy?${query}`, token, {
      cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer', signal: controller.signal,
    }).then(result => {
      if (controller.signal.aborted || owner.current !== scope || readEpoch.current !== epoch) return;
      if (result.account_id !== account.id || result.credential_generation !== account.credential_generation) {
        setReadError(true); return;
      }
      setConnection(result);
    }).catch(() => { if (!controller.signal.aborted && owner.current === scope) setReadError(true); });
    return () => controller.abort();
  }, [scope, requested, disabled]);
  useLayoutEffect(() => {
    onEditingChange?.(editing);
    return () => onEditingChange?.(false);
  }, [editing, onEditingChange]);
  const codex = account.driver === 'openai-codex' && account.auth_kind === 'oauth';
  const canEditProxy = account.can_update_transport_proxy === true;
  const proxyState = account.has_proxy === undefined ? 'connection.proxyUnknown' : account.has_proxy ? account.proxy_scheme ? 'connection.proxyConfigured' : 'connection.proxyNeedsUpdate' : 'connection.proxyMissing';
  const valid = codex ? isPrivateProxyUrl(proxy.trim()) : isGenericPrivateProxyUrl(proxy.trim());
  async function save() {
    if (!valid || saving.current || disabled || !canEditProxy) return;
    saving.current = true;
    readEpoch.current += 1;
    setBusy(true); setError(false); setSaved(false);
    try {
      const updated = await api<UpstreamAccount>(`/internal/v1/upstreams/${encodeURIComponent(account.id)}/transport-proxy`, token, {
        method: 'PUT', headers: { 'Idempotency-Key': crypto.randomUUID() },
        body: JSON.stringify({ tenant_external_id: tenant, proxy_url: proxy.trim(), expected_updated_at: connection?.updated_at ?? account.updated_at, expected_credential_generation: account.credential_generation }),
      });
      if (owner.current !== scope) return;
      setConnection({ account_id: updated.id, proxy_url: proxy.trim(), updated_at: updated.updated_at, credential_generation: updated.credential_generation });
      onSaved?.(updated);
      setProxy(''); setSaved(true);
      await onChanged();
      setEditing(false);
    } catch { if (owner.current === scope) setError(true); }
    finally { saving.current = false; if (owner.current === scope) setBusy(false); }
  }
  return <section className="upstream-connection" aria-label={t('connection.title')}>
    {!embedded && <h3>{t('connection.title')}</h3>}
    <dl>
      {(!embedded || codex) && <div><dt>{t('connection.baseUrl')}</dt><dd><code>{typeof account.config.base_url === 'string' ? account.config.base_url : '—'}</code>{typeof account.config.base_url === 'string' && <CopyButton value={account.config.base_url} label={copy.copyEndpoint} />}{codex && <span className="connection-endpoint-kind">{t('connection.fixed')}</span>}</dd></div>}
      <div><dt>{t('connection.proxy')}</dt><dd><span className={`status ${account.has_proxy && account.proxy_scheme ? 'ok' : 'pending'}`}>{t(account.has_proxy === undefined ? 'connection.proxyUnknown' : account.has_proxy ? proxyState : codex ? proxyState : 'connection.directEgress')}</span>{account.proxy_scheme && <code>{account.proxy_scheme}</code>}{account.has_proxy && account.proxy_scheme && <span>{t(account.proxy_remote_dns ? 'connection.remoteDns' : 'connection.localDns')}</span>}</dd></div>
      {account.proxy_fingerprint && <div><dt>{t('connection.proxyFingerprint')}</dt><dd><DetailTooltip content={account.proxy_fingerprint}><span tabIndex={0}>{t('providerDirectory.account')}</span></DetailTooltip></dd></div>}
    </dl>
    <DetailTooltip content={t('connection.endpointHint')}><span tabIndex={0} className="connection-help">{t('connection.baseUrl')}</span></DetailTooltip>
    {codex && !canEditProxy && <p>{t('connection.proxyAdminOnly')}</p>}
    {!codex && <DetailTooltip content={t('connection.genericProxyHint')}><span tabIndex={0} className="connection-help">{t('connection.proxy')}</span></DetailTooltip>}
    {connection && !editing && <div className="provider-proxy-value">
      {connection.proxy_url === null ? <p>{copy.noProxy}</p> : <ProxyValue value={connection.proxy_url} />}
      {!canEditProxy && connection.proxy_url && <p>{copy.readOnlyProxy}</p>}
    </div>}
    {readError && <p role="status">{copy.readFailed}</p>}
    {!requested && !disabled && <Button type="button" appearance="secondary" onClick={() => setRequested(true)}>{copy.viewProxy}</Button>}
    {canEditProxy && <><Button appearance="secondary" type="button" disabled={disabled || busy} onClick={() => { setEditing(!editing); setProxy(editing ? '' : connection?.proxy_url ?? ''); setError(false); setSaved(false); }}>{t(editing ? 'common.cancel' : 'connection.editProxy')}</Button>
      {editing && <div className="upstream-proxy-editor" onKeyDown={(event) => { if (event.key === 'Enter') { event.preventDefault(); void save(); } }}>
        <ProxyInput value={proxy} onChange={setProxy} disabled={busy} generic={!codex} />
        {proxy && <CopyButton value={proxy} label={copy.copyProxy} />}
        <Button appearance="primary" type="button" onClick={() => void save()} disabled={disabled || busy || !valid}>{t(busy ? 'common.loading' : 'connection.saveProxy')}</Button>
      </div>}
    </>}
    {error && <p className="notice error" role="alert">{t('connection.saveFailed')}</p>}
    {saved && <p role="status">{t('connection.saved')}</p>}
  </section>;
}
