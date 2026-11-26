import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { useOperatorScope } from '../../src/operator/hooks/useOperatorScope';
import { operatorCredentialStorageKey, operatorLoginCycleStorageKey } from '../../src/credentialStorage';

type Action = 'connect' | 'refresh' | 'revalidate' | 'replacement' | 'clear';
declare global {
  interface Window {
    loginCycleFixture: {
      calls: number;
      holdNext: boolean;
      release: (status: number) => void;
      replaceStorage: () => void;
      invoke: (action: Action) => void;
      state: { credential: string; validated: boolean; status: string; cycle?: string };
    };
  }
}
const fixture = window.loginCycleFixture = {
  calls: 0, holdNext: false,
  release: (_status: number) => {},
  // Writing from this document deliberately leaves the hook's storage event
  // pending: this models the other-tab change before its queued event runs.
  replaceStorage: () => {
    localStorage.setItem(operatorCredentialStorageKey, 'synthetic-identity-B');
    localStorage.setItem(operatorLoginCycleStorageKey, 'synthetic-cycle-B');
  },
  invoke: (_action: Action) => {},
  state: { credential: '', validated: false, status: '' } as Window['loginCycleFixture']['state'],
};
function response(status: number) {
  return new Response(JSON.stringify(status === 200
    ? [{ external_id: 'fixture', name: 'Fixture' }]
    : { error: { message: 'Synthetic permission answer' } }), {
    status, headers: { 'Content-Type': 'application/json' },
  });
}
window.fetch = async (input) => {
  const path = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin).pathname;
  if (path !== '/internal/v1/tenants') throw new Error(`Unexpected fixture request ${path}`);
  fixture.calls++;
  if (!fixture.holdNext) return response(200);
  fixture.holdNext = false;
  return new Promise<Response>(resolve => { fixture.release = status => resolve(response(status)); });
};
function Fixture() {
  const scope = useOperatorScope();
  fixture.state = { credential: scope.credential, validated: scope.validated, status: scope.status.kind, cycle: scope.loginCycle };
  fixture.invoke = action => {
    if (action === 'clear') scope.clearCredential();
    else if (action === 'refresh') void scope.refreshTenants();
    else void scope.authenticate(action === 'replacement' ? 'synthetic-candidate-C' : action === 'connect' ? 'synthetic-identity-A' : scope.credential);
  };
  return <output>{scope.status.kind}</output>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
