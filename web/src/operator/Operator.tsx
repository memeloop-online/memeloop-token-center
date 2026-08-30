import { Fragment, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { Shell } from '../components';
import { useI18n } from '../i18n';
import type { UsageAnalysisSessionBucket } from '../types';
import './operator.css';
import type { SessionFocus } from './SessionMonitor';
import { useOperatorScope } from './hooks/useOperatorScope';
import { useOperatorRequestStream } from './hooks/useOperatorRequestStream';
import { CredentialsPage, PricingPage, ProvidersPage, RoutesPage, ServiceCredentialsPage } from './pages/ManagementPages';
import { GenerationsPage, OverviewPage, PluginsPage, UsagePage } from './pages/OperatorPages';
import { RequestsPage } from './pages/RequestsPage';
import { SessionsPage } from './pages/SessionsPage';
import { operatorRouteKeys, type OperatorRouteKey } from './scope/operatorRoutes';

export interface OperatorProps {
  route?: OperatorRouteKey;
  onRouteChange?: (route: OperatorRouteKey) => void;
  embedded?: boolean;
  showNavigation?: boolean;
}

const navigation: Array<{ route: OperatorRouteKey; label: string; domId: string }> = [
  { route: 'overview', label: 'usage.tab.overview', domId: 'overview' },
  { route: 'requests', label: 'nav.traffic', domId: 'traffic' },
  { route: 'sessions', label: 'sessions.sessionsMode', domId: 'sessions' },
  { route: 'usage', label: 'nav.usage', domId: 'usage' },
  { route: 'generations', label: 'nav.generations', domId: 'generations' },
  { route: 'providers', label: 'nav.providers', domId: 'providers' },
  { route: 'routes', label: 'nav.routes', domId: 'routes' },
  { route: 'pricing', label: 'nav.pricing', domId: 'pricing' },
  { route: 'credentials', label: 'nav.credentials', domId: 'credentials' },
  { route: 'service-credentials', label: 'nav.services', domId: 'services' },
  { route: 'plugins', label: 'nav.plugins', domId: 'plugins' },
];

function pageId(route: OperatorRouteKey) {
  return navigation.find((item) => item.route === route)?.domId ?? route;
}

export function Operator({ route, onRouteChange, embedded = false, showNavigation = true }: OperatorProps = {}) {
  const { t } = useI18n();
  const scope = useOperatorScope();
  const [internalRoute, setInternalRoute] = useState<OperatorRouteKey>('requests');
  const [sessionFocus, setSessionFocus] = useState<SessionFocus>();
  const credentialScope = useRef({ credential: '', generation: 0 });
  if (credentialScope.current.credential !== scope.activeCredential) {
    credentialScope.current = {
      credential: scope.activeCredential,
      generation: credentialScope.current.generation + 1,
    };
  }
  const activeRoute = route ?? internalRoute;
  const pageScopeKey = `${credentialScope.current.generation}:${scope.tenant}:${activeRoute}`;
  const stream = useOperatorRequestStream({
    token: scope.activeCredential,
    tenant: scope.tenant,
    enabled: Boolean(scope.activeCredential) && (activeRoute === 'requests' || activeRoute === 'sessions'),
    disconnectedMessage: t('traffic.streamDisconnected'),
  });

  function navigate(next: OperatorRouteKey) {
    if (route === undefined) setInternalRoute(next);
    onRouteChange?.(next);
  }

  function changeRouteByKeyboard(event: KeyboardEvent<HTMLButtonElement>, current: OperatorRouteKey) {
    const currentIndex = operatorRouteKeys.indexOf(current);
    let nextIndex = currentIndex;
    if (event.key === 'ArrowRight') nextIndex = (currentIndex + 1) % operatorRouteKeys.length;
    else if (event.key === 'ArrowLeft') nextIndex = (currentIndex - 1 + operatorRouteKeys.length) % operatorRouteKeys.length;
    else if (event.key === 'Home') nextIndex = 0;
    else if (event.key === 'End') nextIndex = operatorRouteKeys.length - 1;
    else return;
    event.preventDefault();
    const next = operatorRouteKeys[nextIndex];
    navigate(next);
    requestAnimationFrame(() => document.getElementById(`operator-tab-${pageId(next)}`)?.focus());
  }

  function openSession(session: UsageAnalysisSessionBucket) {
    setSessionFocus({ sessionId: session.id, keyId: session.key_id, revision: Date.now() });
    navigate('sessions');
  }

  function openSessionById(sessionId: string) {
    setSessionFocus({ sessionId, revision: Date.now() });
    navigate('sessions');
  }

  let page: ReactNode = null;
  if (scope.activeCredential) {
    const pageProps = { token: scope.activeCredential, tenant: scope.tenant };
    switch (activeRoute) {
      case 'overview': page = <OverviewPage {...pageProps} onNavigate={navigate} onOpenUsageSession={openSession} onOpenSession={openSessionById} />; break;
      case 'requests': page = <RequestsPage {...pageProps} liveEvents={stream.events.current} streamRevision={stream.revision} streamState={stream.state} streamError={stream.error} onOpenSessions={() => navigate('sessions')} onOpenSession={openSessionById} />; break;
      case 'sessions': page = <SessionsPage {...pageProps} focus={sessionFocus} revision={stream.revision} eventKeyIds={stream.sessionEventKeyIds} streamState={stream.state} streamError={stream.error} onOpenRequests={() => navigate('requests')} />; break;
      case 'usage': page = <UsagePage {...pageProps} onOpenSession={openSession} />; break;
      case 'generations': page = <GenerationsPage {...pageProps} />; break;
      case 'providers': page = <ProvidersPage {...pageProps} />; break;
      case 'routes': page = <RoutesPage {...pageProps} />; break;
      case 'pricing': page = <PricingPage {...pageProps} />; break;
      case 'credentials': page = <CredentialsPage {...pageProps} />; break;
      case 'service-credentials': page = <ServiceCredentialsPage {...pageProps} />; break;
      case 'plugins': page = <PluginsPage {...pageProps} />; break;
    }
  }

  const content = <>
    <header className="hero compact">
      <div><span className="eyebrow">{t('operator.eyebrow')}</span><h1>Token Center</h1><p>{t('operator.subtitle')}</p><a className="button secondary portal-link" href="/portal">{t('operator.openPortal')}</a></div>
      <div className="credential operator-credential">
        {scope.tenants.length > 0 && <label className="tenant-picker"><span>{t('operator.tenant')}</span><select value={scope.tenant} onChange={(event) => scope.setTenant(event.target.value)}><option value="">{t('operator.allTenants')}</option>{scope.tenants.map((value) => <option key={value.external_id} value={value.external_id}>{value.external_id}</option>)}</select></label>}
        <input aria-label={t('operator.serviceCredential')} autoComplete="off" type="password" value={scope.credentialInput} onChange={(event) => scope.setCredentialInput(event.target.value)} placeholder={t('operator.tokenPlaceholder')} />
        <button type="button" disabled={!scope.credentialInput.trim()} onClick={() => void scope.authenticate(scope.credentialInput, true)}>{t('common.connect')}</button>
        {scope.credential && <button type="button" className="secondary clear-credential" onClick={scope.clearCredential}>{t('common.clearCredential')}</button>}
      </div>
    </header>
    {scope.authenticating && <div className="console-context"><div><b>{t('common.loading')}</b></div></div>}
    {scope.activeCredential && <div className="console-context"><div><b>{scope.tenant || t('operator.allTenants')}</b><span>{t('common.savedCredentialInUse')}</span></div>{(scope.tenants.length === 0 || !scope.tenant) && <small>{t(scope.tenants.length === 0 ? 'operator.noTenants' : 'operator.selectTenantToWrite')}</small>}</div>}
    {showNavigation && <nav className="tabs" role="tablist" aria-label={t('operator.sections')}>{navigation.map((item) => <button id={`operator-tab-${item.domId}`} role="tab" aria-selected={activeRoute === item.route} aria-controls={`operator-panel-${item.domId}`} tabIndex={activeRoute === item.route ? 0 : -1} key={item.route} className={activeRoute === item.route ? 'active' : ''} onClick={() => navigate(item.route)} onKeyDown={(event) => changeRouteByKeyboard(event, item.route)}>{t(item.label)}</button>)}</nav>}
    {scope.error && <div className="notice error" role="alert">{scope.error}</div>}
    <section id={`operator-panel-${pageId(activeRoute)}`} role="tabpanel" aria-labelledby={showNavigation ? `operator-tab-${pageId(activeRoute)}` : undefined} tabIndex={0}>
      {scope.authenticating ? <div className="empty">{t('common.loading')}</div> : <Fragment key={pageScopeKey}>{page}</Fragment>}
    </section>
  </>;

  return embedded ? content : <Shell operator>{content}</Shell>;
}
