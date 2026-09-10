import type { GroupView, ModelRouteView, UpstreamAccount } from '../types.js';

export type CandidateSource = Pick<ModelRouteView, 'upstream_account_id' | 'upstream_account_ids' | 'included_provider_group_ids' | 'excluded_provider_group_ids'>;

/** Configuration projection, never a health check or an authorization resolver. */
export function routeCandidatePreview(route: CandidateSource, groups: GroupView[], accounts: UpstreamAccount[]) {
  const explicit = new Set([...(route.upstream_account_ids ?? []), ...(route.upstream_account_id ? [route.upstream_account_id] : [])]);
  const includedGroups = groups.filter(group => route.included_provider_group_ids?.includes(group.id));
  const excludedGroups = groups.filter(group => route.excluded_provider_group_ids?.includes(group.id));
  const selected = new Set([...explicit, ...includedGroups.flatMap(group => group.member_ids)]);
  const missingGroupIds = [...new Set([...(route.included_provider_group_ids ?? []), ...(route.excluded_provider_group_ids ?? [])])]
    .filter(id => !groups.some(group => group.id === id));
  const candidates = [...selected].map(id => ({
    id,
    account: accounts.find(account => account.id === id),
    explicit: explicit.has(id),
    includedBy: includedGroups.filter(group => group.member_ids.includes(id)),
    excludedBy: excludedGroups.filter(group => group.member_ids.includes(id)),
  }));
  return {
    candidates,
    missingGroupIds,
    configuredCount: candidates.filter(candidate => candidate.excludedBy.length === 0).length,
    excludedCount: candidates.filter(candidate => candidate.excludedBy.length > 0).length,
  };
}
