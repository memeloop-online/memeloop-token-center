import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { SelfPortal } from '../../src/self/SelfPortal';
import { SelfPortalNavigation } from '../../src/self/SelfPortalNavigation';
import type { SelfPortalRoute } from '../../src/self/routes';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';

function Navigation() {
  const [route, setRoute] = useState<SelfPortalRoute>('overview');
  return <><SelfPortalNavigation activeRoute={route} onNavigate={setRoute} /><output data-testid="route">{route}</output></>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><main style={{ maxWidth: 1320, margin: 'auto', padding: 16 }}>{new URLSearchParams(location.search).has('navigation') ? <Navigation /> : <SelfPortal embedded showNavigation={false} />}</main></MtcFluentProvider></I18nProvider>);
