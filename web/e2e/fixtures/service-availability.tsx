import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { ServiceAvailabilityFooter } from '../../src/app/ServiceAvailabilityFooter';
import '../../src/app-shell.css';

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider>
  <main><h1>Existing application shell</h1></main><ServiceAvailabilityFooter />
</MtcFluentProvider></I18nProvider>);
