import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { ManagedModelSync } from '../../src/operator/ManagedModelSync';
import { ProviderModelCatalog } from '../../src/operator/ProviderModelCatalog';
import type { CatalogRouteAction } from '../../src/operator/managedModelSync';
import { storeCatalogRouteAction } from '../../src/operator/routePrefill';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/providerDirectory.css';

function Fixture() {
  const [tenant, setTenant] = useState('fixture-a');
  const [lastAction, setLastAction] = useState('');
  const [routeCacheRevision, setRouteCacheRevision] = useState(0);
  const routeAction = (action: CatalogRouteAction) => {
    if (new URLSearchParams(window.location.search).get('handoff') === '1') {
      storeCatalogRouteAction('fixture-a', 'browse-account', action);
      window.location.assign('/e2e/fixtures/managed-model-handoff.html');
      return;
    }
    setLastAction(JSON.stringify(action));
  };
  return <I18nProvider><MtcFluentProvider>
    <button type="button" onClick={() => setTenant('fixture-b')}>切换租户</button>
    <ManagedModelSync accountId="managed-account" tenant={tenant} token="fixture-token" onReconciled={() => setRouteCacheRevision((current) => current + 1)} />
    <ProviderModelCatalog accountId="browse-account" tenant="fixture-a" token="fixture-token"
      routeCacheRevision={routeCacheRevision}
      onRouteAction={routeAction} />
    <output id="last-action">{lastAction}</output>
  </MtcFluentProvider></I18nProvider>;
}

createRoot(document.getElementById('root')!).render(<Fixture />);
