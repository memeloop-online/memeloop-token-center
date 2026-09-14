import { useI18n } from '../i18n';
import { MultiCombobox, type ComboboxOption } from './MultiCombobox';
import { JourneyDisclosure } from './JourneyDisclosure';

export function CredentialAuthorizationFields({ routes, groups, routeIds, groupIds, onRoutes, onGroups }: {
  routes: ComboboxOption[]; groups: ComboboxOption[]; routeIds: string[]; groupIds: string[];
  onRoutes: (ids: string[]) => void; onGroups: (ids: string[]) => void;
}) {
  const { t } = useI18n();
  // Keep the full catalog for selected grants; filtering candidates must never
  // change IDs already authorized to this credential.
  const selected = (ids: string[], options: ComboboxOption[]) => ids.map(value => options.find(option => option.value === value) ?? { value, label: `…${value.slice(-6)}`, details: value });
  return <div className="credential-authorization-fields">
    <MultiCombobox label={t('credentials.routeGroups')} options={groups} value={selected(groupIds, groups)} onChange={items => onGroups(items.map(item => item.value))} placeholder={t('credentials.searchRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={name => t('groups.removeMember', { name })} hint={t('credentials.existingGroupsOnly')} />
    <JourneyDisclosure action title={t('credentials.additionalRoutes', { count: routeIds.length })} description={t('credentials.additionalRoutesHint')}>
      <MultiCombobox label={t('credentials.exactRoutes')} options={routes.filter(route => !route.disabled)} value={selected(routeIds, routes)} onChange={items => onRoutes(items.map(item => item.value))} placeholder={t('credentials.searchRoutes')} emptyText={t('groups.noMatches')} removeLabel={name => t('groups.removeMember', { name })} />
    </JourneyDisclosure>
  </div>;
}
