import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { AppShell } from '../../src/app/AppShell';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { GenerationsPage } from '../../src/operator/pages/OperatorPages';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/app-shell.css';
import '../../src/operator/operator.css';

// Renders the generation workspace through the real application shell so
// layout evidence includes the production navigation chrome, not a bare panel.
function Fixture() {
  const [tenant, setTenant] = useState('alpha');
  return <I18nProvider><MtcFluentProvider>
    <AppShell surface="operator" route="generations" onNavigate={() => undefined}>
      <label>Fixture tenant<select value={tenant} onChange={event => setTenant(event.target.value)}><option value="alpha">Alpha</option><option value="beta">Beta</option></select></label>
      <GenerationsPage token="fixture-only" tenant={tenant} writeTenant={tenant} />
    </AppShell>
  </MtcFluentProvider></I18nProvider>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
