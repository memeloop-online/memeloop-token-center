import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { Operator } from '../../src/operator/Operator';
import { pluginRouteKey } from '../../src/app/routes';
import '../../src/styles.css';
import '../../src/theme.css';

localStorage.setItem('mtc.operator.service-credential.v1', 'mts_projection_fixture');
createRoot(document.getElementById('root')!).render(<I18nProvider>
  <Operator route={pluginRouteKey('dashboard', 'summary-page')} embedded showNavigation={false} />
</I18nProvider>);
