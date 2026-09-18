import { Button, DetailTooltip } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { GroupView, ModelRouteView, UpstreamAccount } from '../types';
import './routeList.css';

/** Display the declared source and the server-resolved candidate range separately. */
export function RouteListSource({ route, groups, accounts }: { route: ModelRouteView; groups: GroupView[]; accounts: UpstreamAccount[] }) {
  const { t, locale } = useI18n();
  const groupName = (id: string) => groups.find(group => group.id === id)?.name ?? `${t('routes.listUnknownGroup')} …${id.slice(-6)}`;
  const accountName = (id: string) => accounts.find(account => account.id === id)?.name ?? `${t('routes.listUnknownAccount')} …${id.slice(-6)}`;
  const direct = [...new Set(route.upstream_account_ids ?? (route.upstream_account_id && route.upstream_account_id !== '00000000-0000-0000-0000-000000000000' ? [route.upstream_account_id] : []))];
  const included = route.included_provider_group_ids ?? [];
  const excluded = route.excluded_provider_group_ids ?? [];
  const candidates = route.candidate_upstream_account_ids === undefined ? undefined : [...new Set(route.candidate_upstream_account_ids)];
  const sources = [...direct.map(accountName), ...included.map(groupName)];
  const range = candidates === undefined ? t('routes.listUnknownRange') : t('routes.listCandidateCount', { count: formatNumber(candidates.length, locale) });
  const detail = [
    included.length ? `${t('routes.listIncludedGroups')}: ${included.map(id => `${groupName(id)} (${id})`).join(' · ')}` : '',
    excluded.length ? `${t('routes.listExcludedGroups')}: ${excluded.map(id => `${groupName(id)} (${id})`).join(' · ')}` : '',
    range, ...(candidates ?? []).map(id => `${accountName(id)} (${id})`),
  ].filter(Boolean).join('\n');
  return <div className="route-list-source">
    <span className="route-list-source-name">{sources.length ? sources.join(' · ') : t('routes.listUnknownSource')}</span>
    {route.enabled && candidates?.length === 0 && <DetailTooltip content={t('providerCatalog.routeUnavailableHint')}><span className="status pending" tabIndex={0}>{t('providerCatalog.routeUnavailable')}</span></DetailTooltip>}
    <DetailTooltip content={detail}><Button appearance="subtle" size="small" type="button" className="route-list-source-range">{range}</Button></DetailTooltip>
  </div>;
}
