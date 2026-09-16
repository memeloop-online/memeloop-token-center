import { useI18n } from '../i18n';
import { Tab, TabList } from '../design-system';
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
  return <nav className="self-navigation" aria-label={t('shell.selfService')}><TabList selectedValue={activeRoute} onTabSelect={(_, data) => onNavigate(data.value as SelfPortalRoute)}>{(Object.keys(labels) as SelfPortalRoute[]).map((route) => <Tab key={route} value={route}>{t(labels[route])}</Tab>)}</TabList></nav>;
}
