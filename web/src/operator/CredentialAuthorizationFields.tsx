import { useState } from 'react';
import { useI18n } from '../i18n';
import { MultiCombobox, type ComboboxOption } from './MultiCombobox';
import { JourneyDisclosure } from './JourneyDisclosure';

export function CredentialAuthorizationFields({ routes, groups, routeIds, groupIds, onRoutes, onGroups }: {
  routes: ComboboxOption[]; groups: ComboboxOption[]; routeIds: string[]; groupIds: string[];
  onRoutes: (ids: string[]) => void; onGroups: (ids: string[]) => void;
}) {
  const { t } = useI18n();
  const [routeQuery, setRouteQuery] = useState('');
  const [groupQuery, setGroupQuery] = useState('');
  // Keep the full catalog for selected grants; filtering candidates must never
  // change IDs already authorized to this credential.
  const selected = (ids: string[], options: ComboboxOption[]) => ids.map(value => options.find(option => option.value === value) ?? { value, label: `…${value.slice(-6)}`, details: value });
  // All-selected is a distinct empty state: it requires a nonempty catalog, a
  // blank query, and no remaining selectable option. A true empty catalog and a
  // search miss keep their own explanations.
  const emptyText = (options: ComboboxOption[], picked: ComboboxOption[], query: string, allSelectedKey: string, emptyCatalogKey: string) => {
    if (options.length === 0) return t(emptyCatalogKey);
    if (query.trim()) return t('groups.noMatches');
    const chosen = new Set(picked.map(item => item.value));
    return options.every(option => chosen.has(option.value)) ? t(allSelectedKey) : t('groups.noMatches');
  };
  const routeOptions = routes.filter(route => !route.disabled);
  const selectedRoutes = selected(routeIds, routes);
  const selectedGroups = selected(groupIds, groups);
  const routeSummary = selectedRoutes.map(item => item.label).join(' · ');
  return <div className="credential-authorization-fields">
    <MultiCombobox label={t('credentials.routeGroups')} options={groups} value={selectedGroups} onChange={items => onGroups(items.map(item => item.value))} placeholder={t('credentials.searchRouteGroups')} emptyText={emptyText(groups, selectedGroups, groupQuery, 'credentials.allRouteGroupsSelected', 'credentials.noRouteGroupsAvailable')} removeLabel={name => t('groups.removeMember', { name })} hint={t('credentials.existingGroupsOnly')} onQueryChange={setGroupQuery} />
    <JourneyDisclosure action title={t('credentials.additionalRoutes', { count: routeIds.length })} description={t('credentials.additionalRoutesHint')}>
      <MultiCombobox label={t('credentials.exactRoutes')} options={routeOptions} value={selectedRoutes} onChange={items => onRoutes(items.map(item => item.value))} placeholder={t('credentials.searchRoutes')} emptyText={emptyText(routeOptions, selectedRoutes, routeQuery, 'credentials.allRoutesSelected', 'credentials.noRoutesAvailable')} removeLabel={name => t('groups.removeMember', { name })} onQueryChange={setRouteQuery} />
    </JourneyDisclosure>
    {routeIds.length > 0 && <p className="credential-route-selection-summary" title={t('credentials.selectedRouteSummary', { names: routeSummary })}>{t('credentials.selectedRouteSummary', { names: routeSummary })}</p>}
  </div>;
}
