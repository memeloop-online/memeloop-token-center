import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { CredentialRouteAuthorization } from '../../src/operator/CredentialRouteAuthorization';
import type { GroupView, ModelRouteView } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const routes: ModelRouteView[] = [
  { id: 'single-route-id', public_model: 'Voice model', candidate_upstream_account_ids: ['account-a'] },
  { id: 'shared-route-id', public_model: 'Shared model', candidate_upstream_account_ids: ['account-a', 'account-b'] },
  { id: 'extra-route-id', public_model: 'Additional model', candidate_upstream_account_ids: ['account-b'] },
].map(route => ({ ...route, upstream_model: 'upstream-model', protocol: 'openai', enabled: true, priority: 1, created_at: 1, updated_at: 1, grant_revision: 1 }));
window.fetch = async input => {
  const url = String(input);
  return new Response(JSON.stringify(url.includes('provider-types') ? [{ id: 'fixture', display_name: 'Fixture provider' }] : [
    { id: 'account-a', name: 'primary-account-with-long-name@example.test', driver: 'fixture' },
    { id: 'account-b', name: 'secondary-account@example.test', driver: 'fixture' },
  ]), { status: 200, headers: { 'Content-Type': 'application/json' } });
};
function Fixture() {
  const [ids, setIds] = useState(['single-route-id', 'shared-route-id']);
  return <main style={{ padding: 16, maxWidth: 1100, margin: 'auto' }}>
    <h1>路由展示</h1>
    <CredentialRouteAuthorization token="fixture" tenant="fixture" routes={routes} groups={[]} routeIds={ids} groupIds={[]} onRoutes={setIds} onGroups={() => {}} />
    <output data-testid="grant-ids">{JSON.stringify(ids)}</output>
  </main>;
}
function GroupedFixture() {
  const base = routes[0];
  const entries: ModelRouteView[] = [
    { ...base, id: 'a', public_model: 'Alpha' },
    { ...base, id: 'b', public_model: 'Beta' },
    { ...base, id: 'c', public_model: 'Gamma' },
    { ...base, id: 'shared', public_model: 'Shared', candidate_upstream_account_ids: ['account-b', 'account-a'] },
    { ...base, id: 'unknown', public_model: 'Unknown scope', candidate_upstream_account_ids: undefined, upstream_account_ids: ['account-a'] },
    { ...base, id: 'empty', public_model: 'Empty scope', candidate_upstream_account_ids: [] },
    { ...base, id: 'disabled', public_model: 'Disabled model', enabled: false },
    { ...base, id: 'group-a', public_model: 'Group alpha' },
    { ...base, id: 'group-b', public_model: 'Group beta' },
  ];
  const groups: GroupView[] = [
    { id: 'one', name: 'Same group', member_ids: ['group-a', 'a'], member_count: 2, created_at: 1, updated_at: 1 },
    { id: 'two', name: 'Same group', member_ids: ['group-b', 'a'], member_count: 2, created_at: 1, updated_at: 1 },
  ];
  const [ids, setIds] = useState(['a', 'b', 'c', 'shared', 'unknown', 'empty', 'disabled', 'missing-one', 'missing-two']);
  const [groupIds, setGroupIds] = useState(['one', 'two']);
  return <main style={{ padding: 16, maxWidth: 1100, margin: 'auto' }}>
    <h1>Grant preview</h1>
    <CredentialRouteAuthorization token="fixture" tenant="fixture" routes={entries} groups={groups} routeIds={ids} groupIds={groupIds} onRoutes={setIds} onGroups={setGroupIds} />
    <output data-testid="grant-ids">{JSON.stringify({ ids, groupIds })}</output>
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider>{new URLSearchParams(location.search).has('grouped') ? <GroupedFixture /> : <Fixture />}</MtcFluentProvider></I18nProvider>);
