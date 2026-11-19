import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { PluginsPage } from '../../src/operator/pages/PluginsPage';

function Fixture() {
  const [reloads, setReloads] = useState(0);
  const [tenant, setTenant] = useState('tenant');
  return <I18nProvider><label>Fixture tenant<input value={tenant} onChange={(event) => setTenant(event.target.value)} /></label><PluginsPage token="operator-test" tenant={tenant} catalog={{ kind: 'failed', scopeKey: 'operator-test', message: 'broken current catalog' }} reloadCatalog={async () => { setReloads((value) => value + 1); }} /><p data-testid="reloads">{reloads}</p></I18nProvider>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
