import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { ManagedModelSync } from '../../src/operator/ManagedModelSync';
import { ProviderModelCatalog } from '../../src/operator/ProviderModelCatalog';
import type { CatalogRouteAction } from '../../src/operator/managedModelSync';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/providerDirectory.css';

function Fixture() {
  const [tenant, setTenant] = useState('fixture-a');
  const [lastAction, setLastAction] = useState('');
  return <I18nProvider><MtcFluentProvider>
    <button type="button" onClick={() => setTenant('fixture-b')}>切换租户</button>
    <ManagedModelSync accountId="managed-account" tenant={tenant} token="fixture-token" />
    <ProviderModelCatalog accountId="browse-account" tenant="fixture-a" token="fixture-token"
      onRouteAction={(action: CatalogRouteAction) => setLastAction(JSON.stringify(action))} />
    <output id="last-action">{lastAction}</output>
  </MtcFluentProvider></I18nProvider>;
}

createRoot(document.getElementById('root')!).render(<Fixture />);
