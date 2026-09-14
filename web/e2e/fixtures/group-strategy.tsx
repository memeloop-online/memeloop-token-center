import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { GroupManager } from '../../src/operator/GroupManager';
const kind = new URLSearchParams(location.search).get('kind') === 'credential' ? 'credential' : 'provider';
createRoot(document.getElementById('root')!).render(<I18nProvider><GroupManager kind={kind} token="test" tenant="tenant" resources={[]} onChanged={async () => {}} groups={[{
  id: 'group', name: 'Group', member_ids: [], member_count: 0, created_at: 1, updated_at: 1,
  strategy_version: 2, routing_priority: 0, routing_strategy: null,
}]} /></I18nProvider>);
