import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { PluginsPage } from '../../src/operator/pages/OperatorPages';

function Fixture() {
  const [reloads, setReloads] = useState(0);
  return <I18nProvider><PluginsPage token="operator-test" tenant="tenant" catalog={{ kind: 'failed', scopeKey: 'operator-test', message: 'broken current catalog' }} reloadCatalog={async () => { setReloads((value) => value + 1); }} /><p data-testid="reloads">{reloads}</p></I18nProvider>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
