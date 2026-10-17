import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { Operator, type OperatorProps } from '../../src/operator/Operator';
import { pluginRouteKey } from '../../src/app/routes';
import '../../src/styles.css';
import '../../src/theme.css';

function Fixture() {
  const [route, setRoute] = useState<NonNullable<OperatorProps['route']>>(pluginRouteKey('component-dashboard', 'workspace'));
  const [revision, setRevision] = useState(0);
  return <><button type="button" onClick={() => setRevision(value => value + 1)}>Refresh host view {revision}</button><Operator route={route} onRouteChange={setRoute} embedded showNavigation={false} /></>;
}

localStorage.setItem('mtc.operator.service-credential.v1', 'mts_component_fixture');
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
