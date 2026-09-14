import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { ImageGenerationQuarantine } from '../../src/operator/ImageGenerationQuarantine';
function Fixture() {
  const [tenant, setTenant] = useState('');
  return <I18nProvider><label>Fixture tenant<select value={tenant} onChange={event => setTenant(event.target.value)}><option value="">None</option><option value="alpha">Alpha</option><option value="beta">Beta</option></select></label><ImageGenerationQuarantine token="test" tenant={tenant} writeTenant={tenant} /></I18nProvider>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
