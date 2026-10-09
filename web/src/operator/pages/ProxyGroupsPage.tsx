import { useRef } from 'react';
import { api } from '../../api';
import { Button, LoadingProgress, LoadingState } from '../../design-system';
import { useI18n } from '../../i18n';
import type { UpstreamAccount } from '../../types';
import { useNavigationGuard } from '../../app/NavigationGuard';
import { useOperatorResource } from '../hooks/useOperatorResource';
import { ProxyGroupWorkspace } from '../TransportProxyGroupManager';
import { transportProxyGroupCopy } from '../transportProxyGroupCopy';
import { transportProxyGroupsPath, transportProxyRequest } from '../transportProxyGroups';
import './identityWorkspace.css';

export function ProxyGroupsPage({ token, tenant, initialAccountId, onBack }: {
  token: string;
  tenant: string;
  initialAccountId?: string;
  onBack: () => void;
}) {
  const { locale } = useI18n();
  const copy = transportProxyGroupCopy(locale);
  const closeRequest = useRef<(() => Promise<boolean>) | null>(null);
  useNavigationGuard(async () => closeRequest.current ? closeRequest.current() : true);
  const access = useOperatorResource(Boolean(token), token, async signal => {
    try {
      const value = await transportProxyRequest<{ can_manage: boolean }>(`${transportProxyGroupsPath}/access`, token, { signal });
      if (typeof value?.can_manage !== 'boolean') throw new Error();
      return value;
    } catch { throw new Error(copy.unavailable); }
  }, copy.unavailable);
  const allowed = access.state.kind === 'ready' && !access.state.refreshError && access.state.value.can_manage;
  const accounts = useOperatorResource(allowed && Boolean(tenant), `${token}\0${tenant}`, async signal => {
    try {
      return await api<UpstreamAccount[]>(`/internal/v1/upstreams?${new URLSearchParams({ tenant_external_id: tenant })}`, token, { signal });
    } catch { throw new Error(copy.accountsFailed); }
  }, copy.accountsFailed);
  const accountsValue = allowed && accounts.state.kind === 'ready' ? accounts.state.value : [];
  const requestedAccount = accountsValue.find(account => account.id === initialAccountId
    && account.driver === 'openai-codex' && account.auth_kind === 'oauth' && account.can_update_transport_proxy === true
    && (!account.tenant_external_id || account.tenant_external_id === tenant));
  const accessError = access.state.kind === 'failed' ? copy.unavailable
    : access.state.kind === 'ready' ? access.state.refreshError ? copy.unavailable : !allowed ? copy.denied : '' : '';
  return <section className="panel proxy-groups-page" aria-label={copy.title}>
    <div className="panel-title"><h2>{copy.title}</h2><Button appearance="secondary" type="button" onClick={onBack}>{copy.backToAccounts}</Button></div>
    <LoadingProgress active={Boolean(tenant) && (access.state.kind === 'idle' || access.state.kind === 'loading')} label={copy.checking} level="page" />
    {!tenant ? <p role="status">{copy.selectTenant}</p> : accessError ? <div>
      <p className="notice error" role="alert">{accessError}</p>
      <Button appearance="secondary" type="button" onClick={() => void access.reload()}>{copy.retry}</Button>
    </div> : !allowed ? <LoadingState label={copy.checking} variant="compact" /> : <>
      {(accounts.state.kind === 'failed' || accounts.state.kind === 'ready' && accounts.state.refreshError) && <div>
        <p className="notice error" role="alert">{copy.accountsFailed}</p>
        <Button appearance="secondary" type="button" onClick={() => void accounts.reload()}>{copy.retry}</Button>
      </div>}
      {initialAccountId && accounts.state.kind === 'ready' && !requestedAccount && <p role="alert">{copy.contextMissing}</p>}
      {initialAccountId && (accounts.state.kind === 'idle' || accounts.state.kind === 'loading') ? <LoadingState label={copy.loadingBinding} level="page" variant="compact" />
        : <ProxyGroupWorkspace key={`${token}\0${tenant}\0${requestedAccount?.id ?? ''}`} token={token} tenant={tenant} accounts={accountsValue}
          initialAccountId={requestedAccount?.id} accountsState={accounts.state.kind === 'ready' ? 'ready' : accounts.state.kind === 'failed' ? 'failed' : 'loading'} onChanged={accounts.reload} closeRequest={closeRequest} />}
    </>}
  </section>;
}
