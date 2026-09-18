import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { ProviderModelCatalog } from '../../src/operator/ProviderModelCatalog';
import '../../src/styles.css';
import '../../src/theme.css';

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><ProviderModelCatalog accountId="catalog-account" tenant="fixture" token="fixture-token" disabled={false} /></MtcFluentProvider></I18nProvider>);
