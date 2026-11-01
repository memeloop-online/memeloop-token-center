import type { GroupView, ModelRouteView } from '../types';
import type { ComboboxOption } from './MultiCombobox';

/** Use the server's resolved route range whenever it is available, including an empty range. */
export function strategyAccounts(group: GroupView, kind: 'provider' | 'route', accounts: ComboboxOption[], routes: ModelRouteView[], providerGroups: GroupView[]) {
  const ids = new Set(kind === 'provider' ? group.member_ids : routes.filter(route => group.member_ids.includes(route.id)).flatMap(route => {
    if (route.candidate_upstream_account_ids !== undefined) return route.candidate_upstream_account_ids;
    const excluded = new Set(providerGroups.filter(value => route.excluded_provider_group_ids?.includes(value.id)).flatMap(value => value.member_ids));
    return [...(route.upstream_account_ids ?? (route.upstream_account_id ? [route.upstream_account_id] : [])),
      ...providerGroups.filter(value => route.included_provider_group_ids?.includes(value.id)).flatMap(value => value.member_ids)].filter(id => !excluded.has(id));
  }));
  return accounts.filter(account => ids.has(account.value));
}
