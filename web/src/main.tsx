import { lazy, StrictMode, Suspense } from 'react';
import { createRoot } from 'react-dom/client';
import { AppShell } from './app/AppShell';
import { useAppLocation } from './app/useAppLocation';
import { SelfPortal, type SelfPortalRoute } from './self/SelfPortal';
import type { OperatorRouteKey } from './operator/scope/operatorRoutes';
import { I18nProvider, useI18n } from './i18n';
import './styles.css';
import './theme.css';
import './app-shell.css';
import './styles/metrics.css';
import './styles/request-table.css';

const Operator = lazy(() => import('./operator/Operator').then((module) => ({ default: module.Operator })));

function Loading() {
  const { t } = useI18n();
  return <div className="boot">{t('common.loading')}</div>;
}

function Application() {
  const { surface, route, navigate } = useAppLocation();
  return <AppShell surface={surface} route={route} onNavigate={navigate}>
    {surface === 'operator'
      ? <Suspense fallback={<Loading />}><Operator route={route as OperatorRouteKey} onRouteChange={navigate} embedded showNavigation={false} /></Suspense>
      : <SelfPortal route={route as SelfPortalRoute} onRouteChange={navigate} embedded showNavigation={false} />}
  </AppShell>;
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <I18nProvider>
      <Application />
    </I18nProvider>
  </StrictMode>,
);
