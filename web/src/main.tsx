import { lazy, StrictMode, Suspense, useCallback, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { AppShell } from './app/AppShell';
import { useAppLocation } from './app/useAppLocation';
import { SelfPortal, type SelfPortalRoute } from './self/SelfPortal';
import type { OperatorRouteKey } from './operator/scope/operatorRoutes';
import { I18nProvider, useI18n } from './i18n';
import { MtcFluentProvider } from './design-system/MtcFluentProvider';
import { LoadingState } from './design-system';
import type { PluginNavigationSection } from './operator/pluginContributions';
import './styles.css';
import './theme.css';
import './app-shell.css';
import './styles/metrics.css';
import './styles/request-table.css';
import './sessionViews.css';

const Operator = lazy(() => import('./operator/Operator').then((module) => ({ default: module.Operator })));

function Loading() {
  const { t } = useI18n();
  return <LoadingState label={t('common.loading')} level="page" />;
}

function Application() {
  const { surface, route, context, navigate } = useAppLocation();
  const [pluginNavigation, setPluginNavigation] = useState<PluginNavigationSection[]>([]);
  const updatePluginNavigation = useCallback((next: PluginNavigationSection[]) => setPluginNavigation(next), []);
  return <AppShell surface={surface} route={route} onNavigate={navigate} pluginNavigation={surface === 'operator' ? pluginNavigation : []}>
    {surface === 'operator'
      ? <Suspense fallback={<Loading />}><Operator route={route as OperatorRouteKey} navigationContext={context} onRouteChange={navigate} onPluginNavigation={updatePluginNavigation} embedded showNavigation={false} /></Suspense>
      : <SelfPortal route={route as SelfPortalRoute} onRouteChange={navigate} embedded showNavigation={false} />}
  </AppShell>;
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <I18nProvider>
      <MtcFluentProvider><Application /></MtcFluentProvider>
    </I18nProvider>
  </StrictMode>,
);
