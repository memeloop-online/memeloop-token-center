import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { SelfPortal } from '../../src/self/SelfPortal';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';

function Fixture() {
  const [route, setRoute] = useState<'requests' | 'sessions'>('requests');
  return <I18nProvider><MtcFluentProvider><main><SelfPortal route={route} onRouteChange={(next) => {
    if (next === 'requests' || next === 'sessions') setRoute(next);
  }} embedded /></main></MtcFluentProvider></I18nProvider>;
}

createRoot(document.getElementById('root')!).render(<Fixture />);
