import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { ManagedModelSync } from '../../src/operator/ManagedModelSync';
import { ProviderModelCatalog } from '../../src/operator/ProviderModelCatalog';
import { Operator } from '../../src/operator/Operator';
import { appHref, type OperatorRouteKey } from '../../src/app/routes';
import type { CatalogRouteAction } from '../../src/operator/managedModelSync';
import { storeCatalogRouteAction } from '../../src/operator/routePrefill';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/providerDirectory.css';
import '../../src/operator/operator.css';

function Fixture() {
  const [tenant, setTenant] = useState('fixture-a');
  const [lastAction, setLastAction] = useState('');
  const [routeCacheRevision, setRouteCacheRevision] = useState(0);
  const routeAction = (action: CatalogRouteAction) => {
    const handoff = new URLSearchParams(window.location.search).get('handoff');
    if (handoff === '1' || handoff === 'paged-focus') {
      storeCatalogRouteAction('fixture-a', 'browse-account', action);
      window.location.assign(`/e2e/fixtures/managed-model-handoff.html${handoff === 'paged-focus' ? '?paged-focus=1' : ''}`);
      return;
    }
    setLastAction(JSON.stringify(action));
  };
  return <I18nProvider><MtcFluentProvider>
    <button type="button" onClick={() => setTenant('fixture-b')}>切换租户</button>
    <ManagedModelSync accountId="managed-account" tenant={tenant} token="fixture-token"
      disabled={new URLSearchParams(window.location.search).has('readonly')}
      onReviewModels={() => setLastAction(JSON.stringify({ kind: 'models', accountId: 'managed-account', tenant }))}
      onReviewPricing={() => setLastAction(JSON.stringify({ kind: 'pricing', accountId: 'managed-account', tenant }))}
      onReconciled={() => setRouteCacheRevision((current) => current + 1)} />
    <ProviderModelCatalog accountId="browse-account" tenant="fixture-a" token="fixture-token"
      routeCacheRevision={routeCacheRevision}
      onRouteAction={routeAction} />
    <output id="last-action">{lastAction}</output>
  </MtcFluentProvider></I18nProvider>;
}

function FullProvidersFixture() {
  const [route, setRoute] = useState<OperatorRouteKey>('providers');
  return <I18nProvider><MtcFluentProvider><Operator embedded route={route} onRouteChange={next => {
    if (next.startsWith('plugin--')) return;
    setRoute(next as OperatorRouteKey);
    window.history.pushState({}, '', appHref('operator', next));
  }} /></MtcFluentProvider></I18nProvider>;
}

createRoot(document.getElementById('root')!).render(new URLSearchParams(window.location.search).has('full-page') ? <FullProvidersFixture /> : <Fixture />);
