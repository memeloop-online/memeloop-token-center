import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import RjsfForm from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { api } from '../api';
import { Button } from '../design-system';
import { localizeSchema, useI18n } from '../i18n';
import { schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator } from '../safeValidator';
import { useConfirmDialog } from '../useConfirmDialog';
import type { ProviderType, UpstreamAccount } from '../types';
import { ProxyInput } from './UpstreamConnection';
import { isGenericProxyUrlInput } from './upstreamConnectionPolicy';
import { authorizationCodeCopy } from './authorizationCodeCopy';
import { authorizationStartError, validAuthorizationCallback, type AuthorizationCodeSession } from './authorizationCode';

/** Key by credential, tenant and provider at the call site; no browser persistence. */
export function AuthorizationCodeConnection({ token, tenant, provider, existing, onChanged, onLock }: {
  token: string; tenant: string; provider: ProviderType; existing?: UpstreamAccount;
  onChanged: () => Promise<void>; onLock: (locked: boolean) => void;
}) {
  const { locale, t } = useI18n();
  const copy = authorizationCodeCopy(locale);
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, provider.id]);
  const [name, setName] = useState(provider.display_name);
  const [config, setConfig] = useState<Record<string, unknown>>({});
  const [useProxy, setUseProxy] = useState(false);
  const [proxy, setProxy] = useState('');
  const [proxyScope, setProxyScope] = useState('public');
  const [session, setSession] = useState<AuthorizationCodeSession>();
  const [callback, setCallback] = useState('');
  const [busy, setBusy] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const [notice, setNotice] = useState('');
  const [error, setError] = useState('');
  const inFlight = useRef(false);
  const consumed = useRef(false);
  const live = useRef(true);
  useLayoutEffect(() => { live.current = true; return () => { live.current = false; }; }, []);
  useEffect(() => { onLock(busy || Boolean(session)); }, [busy, session, onLock]);
  if (existing) return <p role="status">{copy.reauthorize}</p>;

  async function start(providerConfig: Record<string, unknown>) {
    if (!token || !tenant || !name.trim() || inFlight.current || session || (useProxy && !isGenericProxyUrlInput(proxy.trim()))) return;
    inFlight.current = true; setBusy(true); setError(''); setNotice('');
    try {
      const result = await api<AuthorizationCodeSession>('/internal/v1/oauth/authorization-code/start', token, {
        method: 'POST', cache: 'no-store', referrerPolicy: 'no-referrer',
        body: JSON.stringify({ tenant_external_id: tenant, account_name: name.trim(), provider_driver: provider.id,
          provider_config: providerConfig, ...(useProxy ? { proxy_url: proxy.trim(), proxy_network_scope: proxyScope } : {}) }),
      });
      if (!live.current) return;
      setSession(result); setProxy('');
    } catch (reason) { if (live.current) setError(copy[authorizationStartError(reason)]); }
    finally { inFlight.current = false; if (live.current) setBusy(false); }
  }

  async function complete() {
    if (!session || inFlight.current || consumed.current) return;
    if (!validAuthorizationCallback(callback)) { setError(copy.invalid); return; }
    inFlight.current = true; consumed.current = true; setSubmitted(true); setBusy(true); setError('');
    const callbackUrl = callback.trim(); setCallback('');
    try {
      const result = await api<UpstreamAccount | { status: 'pending'; retry_after_seconds: number }>('/internal/v1/oauth/authorization-code/complete', token, {
        method: 'POST', cache: 'no-store', referrerPolicy: 'no-referrer',
        body: JSON.stringify({ session_token: session.session_token, callback_url: callbackUrl }),
      });
      if (!live.current) return;
      if ('id' in result) { setSession(undefined); setNotice(copy.saved); }
      else setNotice(copy.pending);
    } catch { if (live.current) setError(copy.uncertain); }
    finally { inFlight.current = false; if (live.current) setBusy(false); }
  }

  return <section className="authorization-form">
    {confirmationDialog}<p className="field-hint">{copy.help}</p>
    <p>{t('providers.provider')}: {provider.display_name} · {t('operator.tenant')}: {tenant}</p>
    <label>{t('providers.name')}<input required maxLength={200} disabled={busy || Boolean(session) || submitted} value={name} onChange={event => setName(event.target.value)} /></label>
    <p>{copy.network}: {useProxy ? `${copy.proxy} · ${proxyScope === 'private' ? copy.private : copy.public}` : copy.direct}</p>
    {!session && !submitted && <>
      <label><input type="checkbox" checked={useProxy} disabled={busy} onChange={event => setUseProxy(event.target.checked)} />{copy.proxy}</label>
      {useProxy && <><ProxyInput generic value={proxy} onChange={setProxy} disabled={busy} /><label>{copy.proxyScope}<select value={proxyScope} disabled={busy} onChange={event => setProxyScope(event.target.value)}><option value="public">{copy.public}</option><option value="private">{copy.private}</option></select></label></>}
      <h3>{copy.config}</h3>
      <RjsfForm schema={localizeSchema(provider.config_schema as RJSFSchema, locale)} formData={config} onChange={({ formData }) => setConfig(formData ?? {})} disabled={busy} validator={safeValidator} templates={schemaFormTemplates} onSubmit={({ formData }) => void start(formData ?? {})}>
        <Button appearance="primary" type="submit" disabled={!token || !tenant || !name.trim() || busy || (useProxy && !isGenericProxyUrlInput(proxy.trim()))}>{t(busy ? 'common.loading' : 'common.startLogin')}</Button>
      </RjsfForm>
    </>}
    {session && !submitted && <>
      <p role="status">{copy.waiting}</p><p>{copy.expires}: {new Date(session.expires_at).toLocaleString(locale)}</p>
      <a className="button secondary" href={session.login_url} target="_blank" rel="noopener noreferrer">{t('common.openAuthorization')}</a>
      <label>{copy.callback}<input type="password" autoComplete="off" spellCheck={false} value={callback} disabled={busy} onChange={event => setCallback(event.target.value)} /></label>
      <p className="field-hint">{copy.callbackHelp}</p>
      <Button appearance="primary" type="button" disabled={busy || !validAuthorizationCallback(callback)} onClick={() => void complete()}>{t('providers.completeAuthorization')}</Button>
    </>}
    {error && <p className="notice error" role="alert">{error}</p>}{notice && <p role="status">{notice}</p>}
    {submitted && <Button type="button" appearance="secondary" disabled={busy} onClick={() => { void onChanged().catch(() => { if (live.current) setError(t('common.requestFailed')); }); }}>{copy.check}</Button>}
    <Button type="button" appearance="secondary" disabled={busy} onClick={async () => {
      if (!await confirm(copy.abandon) || !live.current) return;
      setSession(undefined); setCallback(''); setProxy(''); setConfig({}); setSubmitted(false); consumed.current = false; setError(''); setNotice('');
    }}>{copy.reset}</Button>
  </section>;
}
