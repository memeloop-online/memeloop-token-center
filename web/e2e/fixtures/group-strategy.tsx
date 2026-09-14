import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { api } from '../../src/api';
import type { GroupView } from '../../src/types';
import { I18nProvider } from '../../src/i18n';
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
createRoot(document.getElementById('root')!).render(<I18nProvider>{new URLSearchParams(location.search).has('lifecycle')
  ? <Lifecycle /> : <GroupManager kind={kind} token="test" tenant="tenant" resources={[]} onChanged={async () => {}} groups={[initial]} />}</I18nProvider>);
