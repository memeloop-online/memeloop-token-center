import { lazy, Suspense, type ComponentProps } from 'react';
import { LoadingState } from '../design-system';
import { useI18n } from '../i18n';
import { RequestsPage } from './pages/RequestsPage';
import { RequestWorkspaceTabs } from './RequestWorkspaceTabs';

const GenerationWorkspace = lazy(() => import('./GenerationWorkspace').then((module) => ({ default: module.GenerationWorkspace })));

/** Generation management shares the Requests destination; the legacy link remains valid. */
export function RequestWorkspace({ mode, onModeChange, ...props }: Omit<ComponentProps<typeof RequestsPage>, 'onOpenGenerations'> & {
  mode: 'requests' | 'generations';
  onModeChange: (mode: 'requests' | 'generations') => void;
}) {
  const { t } = useI18n();
  if (mode === 'requests') return <RequestsPage {...props} onOpenGenerations={() => onModeChange('generations')} />;
  return <div className="request-generation-workspace">
    <div className="panel-title traffic-heading"><h2>{t('nav.requests')}</h2><RequestWorkspaceTabs selected="generations" onRequests={() => onModeChange('requests')} onGenerations={() => undefined} onSessions={props.onOpenSessions} /></div>
    <Suspense fallback={<LoadingState label={t('common.loading')} />}><GenerationWorkspace token={props.token} tenant={props.tenant} writeTenant={props.writeTenant} /></Suspense>
  </div>;
}
