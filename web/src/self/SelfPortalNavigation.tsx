import { useI18n } from '../i18n';
import type { SelfPortalRoute } from './routes';

const labels: Record<SelfPortalRoute, string> = {
  overview: 'usage.tab.overview',
  requests: 'self.recent',
  sessions: 'sessions.selfTitle',
  usage: 'usage.title',
  generations: 'self.generations',
  generate: 'self.createGeneration',
};

export function SelfPortalNavigation({ activeRoute, onNavigate }: {
  activeRoute: SelfPortalRoute;
  onNavigate: (route: SelfPortalRoute) => void;
}) {
  const { t } = useI18n();
  return <nav className="self-navigation" aria-label={t('shell.selfService')}><div className="tabs" role="tablist">{(Object.keys(labels) as SelfPortalRoute[]).map((route) => <button key={route} type="button" role="tab" aria-selected={activeRoute === route} tabIndex={activeRoute === route ? 0 : -1} onClick={() => onNavigate(route)}>{t(labels[route])}</button>)}</div></nav>;
}
