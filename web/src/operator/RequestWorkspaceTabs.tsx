import { Tab, TabList } from '@fluentui/react-components';
import { useI18n } from '../i18n';

export function RequestWorkspaceTabs({ selected, onRequests, onGenerations, onSessions }: {
  selected: 'requests' | 'generations' | 'sessions';
  onRequests: () => void;
  onGenerations: () => void;
  onSessions: () => void;
}) {
  const { t } = useI18n();
  return <TabList className="segmented request-workspace-tabs" selectedValue={selected} aria-label={t('sessions.monitorMode')}
    onTabSelect={(_event, data) => {
      if (data.value === 'requests') onRequests();
      if (data.value === 'generations') onGenerations();
      if (data.value === 'sessions') onSessions();
    }}>
    <Tab value="requests">{t('sessions.requestsMode')}</Tab>
    <Tab value="generations">{t('nav.generations')}</Tab>
    <Tab value="sessions">{t('sessions.sessionsMode')}</Tab>
  </TabList>;
}
