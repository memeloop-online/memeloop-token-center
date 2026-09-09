import { Fragment, useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { Shell } from '../components';
import { useI18n } from '../i18n';
import type { PluginManifest, TypedFilterAst, UsageAnalysisSessionBucket } from '../types';
import { api } from '../api';
import './operator.css';
import type { SessionFocus } from './SessionMonitor';
import type { RequestDrilldown } from './overviewDrilldown';
import { useOperatorScope } from './hooks/useOperatorScope';
import { useOperatorResource } from './hooks/useOperatorResource';
import { useOperatorRequestStream } from './hooks/useOperatorRequestStream';
import { CredentialsPage, PricingPage, ProvidersPage, RoutesPage, ServiceCredentialsPage } from './pages/ManagementPages';
import { GenerationsPage, OverviewPage, PluginsPage, UsagePage } from './pages/OperatorPages';
import { RequestsPage } from './pages/RequestsPage';
import { SessionsPage } from './pages/SessionsPage';
import { OperatorAccessSettings, SystemSettingsPage } from './pages/SystemSettingsPage';
import { operatorRouteKeys, isOperatorRouteKey, type OperatorRouteKey } from './scope/operatorRoutes';
import { TenantManager } from './TenantManager';
import {
  PluginContributionPage,
  PluginOverviewCards,
  registerOperatorPluginContributions,
  type PluginNavigationSection,
} from './pluginContributions';
import type { PluginRouteKey } from '../app/routes';

type OperatorApplicationRoute = OperatorRouteKey | PluginRouteKey;

export interface OperatorProps {
  route?: OperatorApplicationRoute;
  onRouteChange?: (route: OperatorApplicationRoute) => void;
  onPluginNavigation?: (navigation: PluginNavigationSection[]) => void;
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
  { route: 'tenants', label: 'nav.tenants', domId: 'tenants' },
  { route: 'credentials', label: 'nav.credentials', domId: 'credentials' },
  { route: 'service-credentials', label: 'nav.services', domId: 'services' },
  { route: 'plugins', label: 'nav.plugins', domId: 'plugins' },
  { route: 'settings', label: 'nav.settings', domId: 'settings' },
];

function pageId(route: OperatorApplicationRoute) {
  return navigation.find((item) => item.route === route)?.domId ?? route;
}

export function Operator({ route, onRouteChange, onPluginNavigation, embedded = false, showNavigation = true }: OperatorProps = {}) {
  const { t } = useI18n();
  const scope = useOperatorScope();
  const [internalRoute, setInternalRoute] = useState<OperatorRouteKey>('requests');
  const [sessionFocus, setSessionFocus] = useState<SessionFocus>();
  const [requestFocus, setRequestFocus] = useState<{ requestId: string; revision: number }>();
  const [requestDrilldown, setRequestDrilldown] = useState<(RequestDrilldown & { token: string; tenant: string })>();
  const requestDrilldownRevision = useRef(0);
  const pluginCatalog = useOperatorResource(
    Boolean(scope.activeCredential), scope.activeCredential,
    (signal) => api<PluginManifest[]>('/internal/v1/plugins', scope.activeCredential, { signal: AbortSignal.any([signal, AbortSignal.timeout(10_000)]) }),
    t('common.requestFailed'),
  );
  const pluginManifests = pluginCatalog.state.kind === 'ready' ? pluginCatalog.state.value : undefined;
  const pluginRegistry = useMemo(() => registerOperatorPluginContributions(pluginManifests ?? []), [pluginManifests]);
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
    // A credential is not a resolved resource scope. Do not open an
    // all-tenant stream while tenant discovery or a scope replacement is in
    // flight; the explicit tenant is part of the stream authorization.
    enabled: Boolean(scope.activeCredential && scope.validated && scope.tenant)
      && (activeRoute === 'requests' || activeRoute === 'sessions'),
    disconnectedMessage: t('traffic.streamDisconnected'),
  });

  useEffect(() => {
    onPluginNavigation?.(pluginRegistry.navigation);
  }, [onPluginNavigation, pluginRegistry]);

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

  function openRequestById(requestId: string) {
    setRequestFocus({ requestId, revision: Date.now() });
    navigate('requests');
  }

  function queueRequestDrilldown(ast: TypedFilterAst) {
    setRequestDrilldown({
      ast,
      revision: ++requestDrilldownRevision.current,
      token: scope.activeCredential,
      tenant: scope.tenant,
    });
  }

  const activeRequestDrilldown = requestDrilldown?.token === scope.activeCredential && requestDrilldown.tenant === scope.tenant
    ? requestDrilldown
    : undefined;

  const accessSettings = <OperatorAccessSettings
    credentialInput={scope.credentialInput}
    credential={scope.credential}
    authenticating={scope.authenticating}
    onCredentialInput={scope.setCredentialInput}
    onConnect={(credential) => { void scope.authenticate(credential); }}
    onClear={scope.clearCredential}
  />;

  let page: ReactNode = null;
  // Pages mount only after authentication and tenant discovery have produced
  // an exact scope. This prevents a transient empty tenant from turning a
  // tenant-scoped resource request into an accidental all-tenant read.
  if (scope.activeCredential && scope.validated && scope.tenant) {
    const pageProps = { token: scope.activeCredential, tenant: scope.tenant, writeTenant: scope.writeTenant };
    if (isOperatorRouteKey(activeRoute)) {
      switch (activeRoute) {
        case 'overview': page = <><OverviewPage {...pageProps} onNavigate={navigate} onRequestDrilldown={queueRequestDrilldown} onOpenUsageSession={openSession} onOpenSession={openSessionById} /><PluginOverviewCards cards={pluginRegistry.overviewCards} token={scope.activeCredential} tenant={scope.tenant} /></>; break;
        case 'requests': page = <RequestsPage {...pageProps} liveEvents={stream.events.current} streamRevision={stream.revision} streamState={stream.state} streamError={stream.error} onOpenSessions={() => navigate('sessions')} onOpenSession={openSessionById} requestFocus={requestFocus} onRequestFocusHandled={(revision) => setRequestFocus((current) => current?.revision === revision ? undefined : current)} requestDrilldown={activeRequestDrilldown} onRequestDrilldownHandled={(revision) => setRequestDrilldown((current) => current?.revision === revision ? undefined : current)} />; break;
        case 'sessions': page = <SessionsPage {...pageProps} focus={sessionFocus} revision={stream.revision} eventKeyIds={stream.sessionEventKeyIds} streamState={stream.state} streamError={stream.error} onOpenRequests={() => navigate('requests')} />; break;
        case 'usage': page = <UsagePage {...pageProps} onOpenSession={openSession} />; break;
        case 'generations': page = <GenerationsPage {...pageProps} />; break;
        case 'providers': page = <ProvidersPage {...pageProps} onOpenRequest={openRequestById} />; break;
        case 'routes': page = <RoutesPage {...pageProps} />; break;
        case 'pricing': page = <PricingPage {...pageProps} />; break;
        case 'tenants': page = <TenantManager token={scope.activeCredential} onChanged={scope.refreshTenants} />; break;
        case 'credentials': page = <CredentialsPage {...pageProps} />; break;
        case 'service-credentials': page = <ServiceCredentialsPage {...pageProps} />; break;
        case 'plugins': page = <PluginsPage {...pageProps} catalog={pluginCatalog.state} />; break;
        case 'settings': page = <>{accessSettings}<SystemSettingsPage {...pageProps} /></>; break;
      }
    } else {
      const registered = pluginRegistry.pages.get(activeRoute);
      page = registered
        ? <PluginContributionPage registered={registered} token={scope.activeCredential} tenant={scope.tenant} />
        : <div className="notice error" role="alert">This plugin page is no longer installed or available.</div>;
    }
  // A settings user may explicitly replace a credential while tenant
  // discovery is still in flight. Keep only that access form mounted; all
  // tenant-scoped pages remain withheld until discovery resolves.
  } else if (!scope.authenticating || activeRoute === 'settings') page = accessSettings;

  const content = <>
    {scope.authenticating && <div className="console-context"><div><b>{t('common.loading')}</b></div></div>}
    {scope.activeCredential && scope.tenants.length === 0 && <div className="console-context"><div><b>{t('operator.noTenants')}</b></div></div>}
    {scope.activeCredential && scope.tenants.length > 1 && <div className="tenant-scope-switcher"><label className="tenant-picker"><span>{t('operator.tenant')}</span><select value={scope.tenant} onChange={(event) => scope.setTenant(event.target.value)}>{scope.tenants.map((value) => <option key={value.external_id} value={value.external_id}>{value.external_id}</option>)}</select></label></div>}
    {showNavigation && <nav className="tabs" role="tablist" aria-label={t('operator.sections')}>{navigation.map((item) => <button id={`operator-tab-${item.domId}`} role="tab" aria-selected={activeRoute === item.route} aria-controls={`operator-panel-${item.domId}`} tabIndex={activeRoute === item.route ? 0 : -1} key={item.route} className={activeRoute === item.route ? 'active' : ''} onClick={() => navigate(item.route)} onKeyDown={(event) => changeRouteByKeyboard(event, item.route)}>{t(item.label)}</button>)}</nav>}
    {scope.error && <div className="notice error" role="alert">{scope.error}</div>}
    <section id={`operator-panel-${pageId(activeRoute)}`} role="tabpanel" aria-labelledby={showNavigation ? `operator-tab-${pageId(activeRoute)}` : undefined} tabIndex={0}>
      {scope.authenticating && activeRoute !== 'settings'
        ? <div className="empty">{t('common.loading')}</div>
        : <Fragment key={pageScopeKey}>{page}</Fragment>}
    </section>
  </>;

  return embedded ? content : <Shell operator>{content}</Shell>;
}
