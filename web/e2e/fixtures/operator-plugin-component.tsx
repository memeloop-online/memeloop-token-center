import { useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { Operator, type OperatorProps } from '../../src/operator/Operator';
import { pluginRouteKey } from '../../src/app/routes';
import { defineOperatorUiPackage, type OperatorUiComponentPropsV1 } from '../../operator-ui-sdk/index.js';
import '../../src/styles.css';
import '../../src/theme.css';

function ComponentWorkspace({ api, contribution, tenantExternalId }: OperatorUiComponentPropsV1) {
  const [status, setStatus] = useState('loading');
  useEffect(() => {
    const controller = new AbortController();
    void api.loadServiceData('health', controller.signal).then((value) => setStatus(String(value.data.status)));
    return () => controller.abort();
  }, [api]);
  return <div data-testid="trusted-plugin-workspace">
    <strong>{contribution.label}</strong>
    <span>{tenantExternalId}</span>
    <span>{status}</span>
    <button type="button" onClick={() => api.navigate('providers')}>Open providers</button>
  </div>;
}

const operatorUiPackage = defineOperatorUiPackage({
  apiVersion: 'operator-ui-package-v1',
  pluginId: 'component-dashboard',
  compatiblePluginVersions: ['1.0.0'],
  components: { workspace: ComponentWorkspace },
});

function Fixture() {
  const [route, setRoute] = useState<NonNullable<OperatorProps['route']>>(pluginRouteKey('component-dashboard', 'workspace'));
  return <Operator
    route={route}
    onRouteChange={setRoute}
    operatorUiPackages={[operatorUiPackage]}
    embedded
    showNavigation={false}
  />;
}

localStorage.setItem('mtc.operator.service-credential.v1', 'mts_component_fixture');
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
