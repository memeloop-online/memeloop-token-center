import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { GroupView, UpstreamAccount } from '../types';
import { routeCandidatePreview, type CandidateSource } from './routeCandidatePreview';

export function RouteCandidateSummary({ route, groups, accounts }: { route: CandidateSource; groups: GroupView[]; accounts: UpstreamAccount[] }) {
  const { locale, t } = useI18n();
  const preview = routeCandidatePreview(route, groups, accounts);
  return <details className="route-candidate-summary">
    <summary>{t('routing.candidateCounts', { configured: formatNumber(preview.configuredCount, locale), excluded: formatNumber(preview.excludedCount, locale) })}</summary>
    <p className="field-hint">{t('routing.configNotHealth')}</p>
    {preview.missingGroupIds.length > 0 && <p className="notice warning compact">{t('routing.missingGroups', { count: formatNumber(preview.missingGroupIds.length, locale) })}</p>}
    {preview.candidates.length === 0 ? <p className="field-hint">{t('routes.selectCandidatesFirst')}</p> : <ul>
      {preview.candidates.map(candidate => <li key={candidate.id}>
        <b>{candidate.account?.name ?? t('modelPicker.unknown')}</b>
        <small>{candidate.account?.driver ?? candidate.id}</small>
        <span>{candidate.explicit ? t('routing.explicitSource') : t('routing.groupSource')}
          {candidate.includedBy.length > 0 && ` · ${candidate.includedBy.map(group => group.name).join(' · ')}`}
        </span>
        <span className={candidate.excludedBy.length > 0 ? 'field-error' : 'field-hint'}>
          {candidate.excludedBy.length > 0
            ? t('routing.excludedBy', { groups: candidate.excludedBy.map(group => group.name).join(' · ') })
            : !candidate.account ? t('routing.accountUnavailable')
              : candidate.account.status === 'active' ? t('routing.activeNotHealth') : t('routing.accountInactive')}
        </span>
      </li>)}
    </ul>}
    <p className="field-hint">{t('routing.priorityOrder')}</p>
  </details>;
}
