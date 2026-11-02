import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { api } from '../../src/api';
import type { GroupView } from '../../src/types';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import '../../src/styles.css';
import { GroupManager } from '../../src/operator/GroupManager';
const kind = new URLSearchParams(location.search).get('kind') === 'credential' ? 'credential' : 'provider';
const initial: GroupView = {
  id: 'group', name: 'Group', member_ids: [], member_count: 0, created_at: 1, updated_at: 1,
  strategy_version: 2, routing_priority: 0, routing_strategy: null,
};
function Lifecycle() {
  const [tenant, setTenant] = useState('tenant');
  const [groups, setGroups] = useState<GroupView[]>([{ ...initial, name: 'CSiL', member_ids: ['sol', 'terra', 'luna'], member_count: 3 }]);
  return <><button onClick={() => { setTenant('other'); setGroups([{ ...initial, id: 'other-group', name: 'Other tenant' }]); }}>Switch tenant</button>
    <GroupManager kind="provider" token="test" tenant={tenant} groups={groups}
      resources={['sol', 'terra', 'luna', 'kimi'].map(value => ({ value, label: value }))}
      onChanged={async () => setGroups(await api<GroupView[]>(`/internal/v1/provider-groups?tenant_external_id=${tenant}`, 'test'))} /></>;
}
function StrategyFixture() {
  const [group, setGroup] = useState(initial);
  return <><button onClick={() => setGroup({ ...initial, name: 'Group externally updated', updated_at: 3, strategy_version: 4, routing_strategy: { plugin_id: 'weighted', config: { factor: 99 } } })}>External strategy update</button>
    <GroupManager kind={kind} token="test" tenant="tenant" resources={[]} onChanged={async () => {}} groups={[group]} /></>;
}
function ResourceFixture() {
  const [group, setGroup] = useState<GroupView>({ ...initial, member_ids: ['alpha', 'beta', 'gamma'], member_count: 3 });
  return <><button onClick={() => setGroup(current => ({ ...current, member_ids: ['alpha', 'gamma'], updated_at: current.updated_at + 1 }))}>Remove Beta membership</button>
    <GroupManager kind="provider" token="test" tenant="tenant" groups={[group]} onChanged={async () => {}}
      resources={[{ value: 'alpha', label: 'Alpha account', description: 'OpenAI' }, { value: 'beta', label: 'Beta account', description: 'Anthropic' }, { value: 'gamma', label: 'Gamma account', description: 'Google' }, { value: 'outside', label: 'Outside group' }]} /></>;
}
createRoot(document.getElementById('root')!).render(<MtcFluentProvider><I18nProvider>{new URLSearchParams(location.search).has('resources')
  ? <ResourceFixture /> : new URLSearchParams(location.search).has('lifecycle') ? <Lifecycle /> : <StrategyFixture />}</I18nProvider></MtcFluentProvider>);
