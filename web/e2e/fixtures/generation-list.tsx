import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { GenerationsPage } from '../../src/self/GenerationsPage';
import type { KeyView } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';

const key = { key_id: 'fixture-key', currency: 'USD' } as KeyView;
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><GenerationsPage credential="fixture-only" credentialView={key} onError={() => undefined} /></MtcFluentProvider></I18nProvider>);
