import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { SelfPortal } from '../../src/self/SelfPortal';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';

createRoot(document.getElementById('root')!).render(
  <I18nProvider><MtcFluentProvider><main><SelfPortal route="requests" embedded showNavigation={false} /></main></MtcFluentProvider></I18nProvider>,
);
