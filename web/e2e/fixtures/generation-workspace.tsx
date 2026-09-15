import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { GenerationWorkspace } from '../../src/operator/GenerationWorkspace';
import '../../src/styles.css';
import '../../src/theme.css';

createRoot(document.getElementById('root')!).render(
  <I18nProvider><MtcFluentProvider><GenerationWorkspace token="fixture-only" tenant="alpha" writeTenant="alpha" /></MtcFluentProvider></I18nProvider>,
);
