import { lazy, Suspense, useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { api } from '../api';
import { Shell } from '../components';
import { clearRememberedCredential, readRememberedCredential, rememberCredential } from '../credentialStorage';
import { useI18n } from '../i18n';
import type { KeyView, RequestDetail, RequestView } from '../types';
import { selfErrorMessage } from './errors';
import { type SelfPortalRoute } from './routes';
import { SelfPortalNavigation } from './SelfPortalNavigation';
import { RequestDetailDrawer } from './SelfDrawers';

const GeneratePage = lazy(() => import('./GeneratePage').then((module) => ({ default: module.GeneratePage })));
const GenerationsPage = lazy(() => import('./GenerationsPage').then((module) => ({ default: module.GenerationsPage })));
const OverviewPage = lazy(() => import('./OverviewPage').then((module) => ({ default: module.OverviewPage })));
const RequestsPage = lazy(() => import('./RequestsPage').then((module) => ({ default: module.RequestsPage })));
const SessionsPage = lazy(() => import('./SessionsPage').then((module) => ({ default: module.SessionsPage })));
const UsagePage = lazy(() => import('./UsagePage').then((module) => ({ default: module.UsagePage })));

export type { SelfPortalRoute } from './routes';
export { isSelfPortalRoute, selfPortalRouteFromSearch, selfPortalRoutes, selfPortalSearchForRoute } from './routes';

export interface SelfPortalProps {
  /** Controlled page key. Omit it to let the portal own its active page. */
  route?: SelfPortalRoute;
  /** Receives navigation intent in both controlled and uncontrolled modes. */
  onRouteChange?: (route: SelfPortalRoute) => void;
  /** Hide local tabs when a surrounding application shell owns navigation. */
  showNavigation?: boolean;
  /** Render content only when a surrounding application shell already provides Shell. */
  embedded?: boolean;
}

export function SelfPortal({ route, onRouteChange, showNavigation = true, embedded = false }: SelfPortalProps = {}) {
  const { t } = useI18n();
  const [internalRoute, setInternalRoute] = useState<SelfPortalRoute>('overview');
  const activeRoute = route ?? internalRoute;
  const [credential, setCredential] = useState(() => readRememberedCredential('self'));
  const [credentialInput, setCredentialInput] = useState('');
  const [credentialView, setCredentialView] = useState<KeyView>();
  const [requestDetail, setRequestDetail] = useState<RequestDetail>();
  const [sessionFocus, setSessionFocus] = useState<string>();
  const [error, setError] = useState('');
  const [authenticating, setAuthenticating] = useState(false);
  const [credentialScopeGeneration, setCredentialScopeGeneration] = useState(0);
  const credentialScopeRef = useRef(0);
  const authSequence = useRef(0);
  const authController = useRef<AbortController | undefined>(undefined);
  const detailSequence = useRef(0);
  const detailController = useRef<AbortController | undefined>(undefined);

  function navigate(next: SelfPortalRoute) {
    if (route === undefined) setInternalRoute(next);
    setError('');
    onRouteChange?.(next);
  }

  async function authenticate(value: string, replaceCredential: boolean) {
    const nextCredential = value.trim();
    if (!nextCredential) return;
    const sequence = ++authSequence.current;
    authController.current?.abort();
    const controller = new AbortController();
    authController.current = controller;
    setAuthenticating(true);
    setError('');
    try {
      const view = await api<KeyView>('/self/v1/key', nextCredential, { signal: controller.signal });
      if (sequence !== authSequence.current || controller.signal.aborted) return;
      detailSequence.current += 1;
      detailController.current?.abort();
      setRequestDetail(undefined);
      setSessionFocus(undefined);
      rememberCredential('self', nextCredential);
      setCredential(nextCredential);
      setCredentialView(view);
      credentialScopeRef.current += 1;
      setCredentialScopeGeneration((current) => current + 1);
      if (replaceCredential) setCredentialInput('');
    } catch (reason) {
      if (sequence !== authSequence.current || controller.signal.aborted) return;
      setCredentialView(undefined);
      setError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (sequence === authSequence.current) setAuthenticating(false);
    }
  }

  function submitCredential(event: FormEvent) {
    event.preventDefault();
    void authenticate(credentialInput, true);
  }

  function clearCredential() {
    authSequence.current += 1;
    authController.current?.abort();
    detailSequence.current += 1;
    detailController.current?.abort();
    clearRememberedCredential('self');
    setCredential('');
    setCredentialInput('');
    setCredentialView(undefined);
    setRequestDetail(undefined);
    setSessionFocus(undefined);
    setError('');
    setAuthenticating(false);
    credentialScopeRef.current += 1;
    setCredentialScopeGeneration((current) => current + 1);
    setInternalRoute('overview');
    onRouteChange?.('overview');
  }

  async function openRequest(request: RequestView) {
    const sequence = ++detailSequence.current;
    detailController.current?.abort();
    const controller = new AbortController();
    detailController.current = controller;
    const expectedCredential = credential;
    const expectedScope = credentialScopeRef.current;
    try {
      const detail = await api<RequestDetail>(`/self/v1/requests/${request.request_id}`, expectedCredential, { signal: controller.signal });
      if (sequence === detailSequence.current && !controller.signal.aborted && expectedScope === credentialScopeRef.current) setRequestDetail(detail);
    } catch (reason) {
      if (sequence === detailSequence.current && !controller.signal.aborted) setError(selfErrorMessage(reason, t, t('self.detailFailed')));
    }
  }

  function openSession(sessionId: string) {
    setSessionFocus(sessionId);
    navigate('sessions');
  }

  useEffect(() => {
    if (credential) void authenticate(credential, false);
    return () => {
      authSequence.current += 1;
      authController.current?.abort();
      detailSequence.current += 1;
      detailController.current?.abort();
    };
  }, []);

  let page: ReactNode = null;
  if (credentialView) {
    const pageProps = { credential, credentialView, onError: setError };
    switch (activeRoute) {
      case 'overview':
        page = <OverviewPage {...pageProps} onOpenRequest={(request) => void openRequest(request)} onOpenSession={openSession} />;
        break;
      case 'requests':
        page = <RequestsPage {...pageProps} onOpenRequest={(request) => void openRequest(request)} onOpenSession={openSession} />;
        break;
      case 'sessions':
        page = <SessionsPage {...pageProps} focusSessionId={sessionFocus} onOpenRequest={(request) => void openRequest(request)} />;
        break;
      case 'usage':
        page = <UsagePage {...pageProps} />;
        break;
      case 'generations':
        page = <GenerationsPage {...pageProps} />;
        break;
      case 'generate':
        page = <GeneratePage credential={credential} onError={setError} />;
        break;
    }
  }
  const credentialScopeKey = credentialView
    ? `${credentialView.key_id}:${credentialView.credential_generation}:${credentialScopeGeneration}`
    : `signed-out:${credentialScopeGeneration}`;

  const content = <div className="self-portal" data-self-route={activeRoute}>
    {!credentialView ? <header className="hero self-sign-in"><div><h1>{t('self.title')}</h1></div><form className="credential" onSubmit={submitCredential}><label><span>{t('self.credential')}</span><input autoComplete="off" type="password" value={credentialInput} onChange={(event) => setCredentialInput(event.target.value)} placeholder={t('self.placeholder')} /></label><button type="submit" disabled={authenticating || !credentialInput.trim()}>{authenticating ? t('common.loading') : t('common.load')}</button>{credential && <button type="button" className="secondary clear-credential" onClick={clearCredential}>{t('common.clearCredential')}</button>}</form></header> : <>
      <div className="console-context self-account-status"><div><b>{credentialView.alias}</b><span>{t('common.savedCredentialInUse')}</span></div><button type="button" className="secondary clear-credential" onClick={clearCredential}>{t('common.clearCredential')}</button></div>
      {showNavigation && <SelfPortalNavigation activeRoute={activeRoute} onNavigate={navigate} />}
    </>}
    {error && <div className="notice error" role="alert">{error}</div>}
    {page && <Suspense key={credentialScopeKey} fallback={<div className="boot">{t('common.loading')}</div>}>{page}</Suspense>}
    {requestDetail && <RequestDetailDrawer detail={requestDetail} currency={credentialView?.currency} onClose={() => setRequestDetail(undefined)} />}
  </div>;

  return embedded ? content : <Shell>{content}</Shell>;
}
