import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import RjsfForm from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { api } from '../api';
import { Button, Checkbox, Input } from '../design-system';
import { localizeSchema, useI18n } from '../i18n';
import { tenantDisplayName } from '../tenantDisplayName';
import { schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator } from '../safeValidator';
import { useConfirmDialog } from '../useConfirmDialog';
import type { ProviderType, UpstreamAccount } from '../types';
import { ProxyInput } from './UpstreamConnection';
import { isGenericProxyUrlInput } from './upstreamConnectionPolicy';
import { authorizationCodeCopy } from './authorizationCodeCopy';
import { authorizationStartError, canReauthorizeAccount, isAuthorizationIdentityMismatch, validAuthorizationCallback, type AuthorizationCodeSession } from './authorizationCode';
import { fluentFormWidgets } from './FluentFormWidgets';

/** Key by credential, tenant and provider at the call site; no browser persistence. */
export function AuthorizationCodeConnection({ token, tenant, provider, existing, onChanged, onLock }: {
  token: string; tenant: string; provider: ProviderType; existing?: UpstreamAccount;
  onChanged: () => Promise<void>; onLock: (locked: boolean) => void;
}) {
  const { locale, t } = useI18n();
  const copy = authorizationCodeCopy(locale, provider.id, Boolean(existing));
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, provider.id]);
  const [name, setName] = useState(existing?.name ?? provider.display_name);
  const [config, setConfig] = useState<Record<string, unknown>>({});
  const [useProxy, setUseProxy] = useState(false);
  const [proxy, setProxy] = useState('');
  const [session, setSession] = useState<AuthorizationCodeSession>();
  const [recoveryDeadline, setRecoveryDeadline] = useState<number>();
  const [callback, setCallback] = useState('');
  const [busy, setBusy] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const saved = useRef(false);
  const [notice, setNotice] = useState('');
  const [error, setError] = useState('');
  const inFlight = useRef(false);
  const consumed = useRef(false);
  const live = useRef(true);
  useLayoutEffect(() => { live.current = true; return () => { live.current = false; }; }, []);
  useEffect(() => { onLock(busy || Boolean(session)); }, [busy, session, onLock]);
  if (existing && !canReauthorizeAccount(existing, provider)) return <p role="status">{copy.reauthorize}</p>;

  async function start(providerConfig: Record<string, unknown>) {
    if (!token || !tenant || !name.trim() || inFlight.current || session || (useProxy && !isGenericProxyUrlInput(proxy.trim()))) return;
    inFlight.current = true; setBusy(true); setError(''); setNotice('');
    try {
      const result = await api<AuthorizationCodeSession>('/internal/v1/oauth/authorization-code/start', token, {
        method: 'POST', cache: 'no-store', referrerPolicy: 'no-referrer',
        body: JSON.stringify({ tenant_external_id: tenant, account_name: existing?.name ?? name.trim(), provider_driver: provider.id,
          ...(existing ? { upstream_account_id: existing.id } : { provider_config: providerConfig,
            ...(useProxy ? { proxy_url: proxy.trim(), proxy_network_scope: 'private' } : {}) }) }),
      });
      if (!live.current) return;
      setSession(result); setRecoveryDeadline(result.recovery_expires_at); setProxy('');
    } catch (reason) { if (live.current) setError(copy[authorizationStartError(reason)]); }
    finally { inFlight.current = false; if (live.current) setBusy(false); }
  }

  async function complete(continueIssued = false) {
    if (!session || inFlight.current || (continueIssued ? !submitted : consumed.current)) return;
    if (continueIssued && (recoveryDeadline === undefined || Date.now() >= recoveryDeadline)) { setError(copy.continueExpired); return; }
    if (!continueIssued && !validAuthorizationCallback(callback)) { setError(copy.invalid); return; }
    inFlight.current = true; consumed.current = true; setSubmitted(true); setBusy(true); setError(''); setNotice('');
    // Explicit continuation can only complete already-issued server state. Never retain or replay a code.
    const callbackUrl = continueIssued ? '' : callback.trim(); setCallback('');
    try {
      const result = await api<UpstreamAccount | { status: 'pending'; retry_after_seconds: number }>('/internal/v1/oauth/authorization-code/complete', token, {
        method: 'POST', cache: 'no-store', referrerPolicy: 'no-referrer',
        body: JSON.stringify({ session_token: session.session_token, callback_url: callbackUrl }),
      });
      if (!live.current) return;
      if ('id' in result) {
        saved.current = true; setSession(undefined); setNotice(existing ? copy.reauthorized : copy.saved);
        // Account persistence succeeded. A list-read failure is not an OAuth failure.
        try { await onChanged(); }
        catch { if (live.current) setError(copy.savedButReadFailed); }
      }
      else setNotice(copy.pending);
    } catch (reason) {
      if (live.current) {
        if (existing && isAuthorizationIdentityMismatch(reason)) {
          setSession(undefined); setRecoveryDeadline(undefined); setError(copy.identityMismatch);
        } else setError(continueIssued ? copy.continueFailed : copy.uncertain);
      }
    }
    finally { inFlight.current = false; if (live.current) setBusy(false); }
  }

  return <section className="authorization-form">
    {confirmationDialog}<p className="field-hint">{existing ? copy.reauthorizeHelp : copy.help}</p>
    <p>{t('providers.provider')}: {provider.display_name} · {t('operator.tenant')}: {tenantDisplayName(tenant, locale)}</p>
    <label>{t('providers.name')}<Input required maxLength={200} disabled={Boolean(existing) || busy || Boolean(session) || submitted} value={name} onChange={event => setName(event.target.value)} /></label>
    <p>{copy.network}: {existing ? copy.retainedNetwork : useProxy ? `${copy.proxy} · ${copy.private}` : copy.direct}</p>
    {existing && !session && !submitted && <Button appearance="primary" type="button" disabled={!token || !tenant || busy} onClick={() => void start({})}>{t(busy ? 'common.loading' : 'common.startLogin')}</Button>}
    {!existing && !session && !submitted && <>
      <Checkbox label={copy.proxy} checked={useProxy} disabled={busy} onChange={(_, data) => setUseProxy(data.checked === true)} />
      {useProxy && <ProxyInput generic value={proxy} onChange={setProxy} disabled={busy} />}
      <h3>{copy.config}</h3>
      <RjsfForm schema={localizeSchema(provider.config_schema as RJSFSchema, locale)} formData={config} onChange={({ formData }) => setConfig(formData ?? {})} disabled={busy} validator={safeValidator} templates={schemaFormTemplates} widgets={fluentFormWidgets} onSubmit={({ formData }) => void start(formData ?? {})}>
        <Button appearance="primary" type="submit" disabled={!token || !tenant || !name.trim() || busy || (useProxy && !isGenericProxyUrlInput(proxy.trim()))}>{t(busy ? 'common.loading' : 'common.startLogin')}</Button>
      </RjsfForm>
    </>}
    {session && !submitted && <>
      <p role="status">{copy.waiting}</p><p>{copy.expires}: {new Date(session.expires_at).toLocaleString(locale)}</p>
      <a className="button secondary" href={session.login_url} target="_blank" rel="noopener noreferrer">{t('common.openAuthorization')}</a>
      <label>{copy.callback}<Input type="password" autoComplete="off" spellCheck={false} value={callback} disabled={busy} onChange={event => setCallback(event.target.value)} /></label>
      <p className="field-hint">{copy.callbackHelp}</p>
      <Button appearance="primary" type="button" disabled={busy || !validAuthorizationCallback(callback)} onClick={() => void complete()}>{t('providers.completeAuthorization')}</Button>
    </>}
    {recoveryDeadline !== undefined && <div className="field-hint"><p>{copy.recoveryExpires}: {new Date(recoveryDeadline).toLocaleString(locale)}</p>{!saved.current && <p>{copy.recoveryHint}</p>}</div>}
    {error && <p className="notice error" role="alert">{error}</p>}{notice && <p role="status">{notice}</p>}
    {submitted && session && <><p className="field-hint">{copy.continueHelp}</p><Button type="button" appearance="secondary" disabled={busy} onClick={() => void complete(true)}>{copy.continueSetup}</Button></>}
    {submitted && <Button type="button" appearance="secondary" disabled={busy} onClick={async () => {
      if (inFlight.current) return;
      inFlight.current = true; setBusy(true); setError('');
      try { await onChanged(); }
      catch { if (live.current) setError(saved.current ? copy.savedButReadFailed : t('common.requestFailed')); }
      finally { inFlight.current = false; if (live.current) setBusy(false); }
    }}>{copy.check}</Button>}
    <Button type="button" appearance="secondary" disabled={busy} onClick={async () => {
      if (!await confirm(copy.abandon) || !live.current) return;
      setSession(undefined); setRecoveryDeadline(undefined); setCallback(''); setProxy(''); setConfig({}); setSubmitted(false); consumed.current = false; saved.current = false; setError(''); setNotice('');
    }}>{copy.reset}</Button>
  </section>;
}
