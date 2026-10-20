import { ToggleButton } from '@fluentui/react-components';
import { useI18n } from '../i18n';

export function RequestWorkspaceTabs({ selected, onRequests, onGenerations, onSessions }: {
  selected: 'requests' | 'generations';
  onRequests: () => void;
  onGenerations: () => void;
  onSessions: () => void;
}) {
  const { t } = useI18n();
  return <div className="segmented" role="group" aria-label={t('sessions.monitorMode')}>
    <ToggleButton appearance="subtle" checked={selected === 'requests'} onClick={onRequests}>{t('sessions.requestsMode')}</ToggleButton>
    <ToggleButton appearance="subtle" checked={selected === 'generations'} onClick={onGenerations}>{t('nav.generations')}</ToggleButton>
    <ToggleButton appearance="subtle" checked={false} onClick={onSessions}>{t('sessions.sessionsMode')}</ToggleButton>
  </div>;
}
