import { Button, DetailTooltip } from '../design-system';
import { CopyButton } from '../CopyButton';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { GroupView, ModelRouteView, ProviderType, UpstreamAccount } from '../types';
import { canonicalProviderDriver, providerDisplayName } from './providerDisplayName';
import './routeList.css';

/** Display declared sources and the server-resolved candidate range separately. */
export function RouteListSource({ route, groups, accounts, providers = [] }: {
  route: ModelRouteView; groups: GroupView[]; accounts: UpstreamAccount[]; providers?: ProviderType[];
}) {
  const { t, locale } = useI18n();
  const zh = locale.startsWith('zh');
  const copy = zh ? { accountId: '账号 ID', groupId: '组 ID', routeId: '路由 ID', copyId: '复制 ID' }
    : { accountId: 'Account ID', groupId: 'Group ID', routeId: 'Route ID', copyId: 'Copy ID' };
  const groupName = (id: string) => groups.find(group => group.id === id)?.name ?? t('routes.listUnknownGroup');
  const accountName = (id: string) => {
    const account = accounts.find(account => account.id === id);
    if (!account) return t('routes.listUnknownAccount');
    const provider = providers.find(provider => provider.id === canonicalProviderDriver(account.driver));
    const name = provider ? providerDisplayName(account.driver, providers, locale) : undefined;
    let endpoint: string | undefined;
    if (accounts.some(other => other.id !== id && other.name === account.name) && typeof account.config.base_url === 'string') {
      try { endpoint = new URL(account.config.base_url).host; } catch { /* An invalid endpoint is not a readable account label. */ }
    }
    return [account.name, name, endpoint].filter(Boolean).join(' · ');
  };
  const direct = [...new Set(route.upstream_account_ids ?? (route.upstream_account_id && route.upstream_account_id !== '00000000-0000-0000-0000-000000000000' ? [route.upstream_account_id] : []))];
  const included = route.included_provider_group_ids ?? [];
  const excluded = route.excluded_provider_group_ids ?? [];
  const candidates = route.candidate_upstream_account_ids === undefined ? undefined : [...new Set(route.candidate_upstream_account_ids)];
  const sources = [...direct.map(accountName), ...included.map(groupName)];
  const range = candidates === undefined ? t('routes.listUnknownRange') : t('routes.listCandidateCount', { count: formatNumber(candidates.length, locale) });
  const detail = [
    included.length ? `${t('routes.listIncludedGroups')}: ${included.map(groupName).join(' · ')}` : '',
    excluded.length ? `${t('routes.listExcludedGroups')}: ${excluded.map(groupName).join(' · ')}` : '',
    range, ...(candidates ?? []).map(accountName),
  ].filter(Boolean).join('\n');
  const technical = [
    { label: copy.routeId, id: route.id },
    ...[...new Set([...direct, ...(candidates ?? [])])].map(id => ({ label: `${accountName(id)} · ${copy.accountId}`, id })),
    ...[...new Set([...included, ...excluded])].map(id => ({ label: `${groupName(id)} · ${copy.groupId}`, id })),
  ];
  return <div className="route-list-source">
    <span className="route-list-source-name">{sources.length ? sources.join(' · ') : t('routes.listUnknownSource')}</span>
    {route.enabled && candidates?.length === 0 && <DetailTooltip content={t('providerCatalog.routeUnavailableHint')}><span className="status pending" tabIndex={0}>{t('providerCatalog.routeUnavailable')}</span></DetailTooltip>}
    <DetailTooltip content={detail}><Button appearance="subtle" size="small" type="button" className="route-list-source-range">{range}</Button></DetailTooltip>
    <details className="route-source-diagnostics"><summary>{t('request.technicalDetails')}</summary><dl>{technical.map(item => <div key={`${item.label}\0${item.id}`}><dt>{item.label}</dt><dd><code>{item.id}</code><CopyButton fluent value={item.id} label={`${copy.copyId}: ${item.label}`} /></dd></div>)}</dl></details>
  </div>;
}
