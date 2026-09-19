import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RequestRefreshControl } from '../../src/operator/traffic/RequestRefreshControl';
import { useRequestRefreshPreference } from '../../src/operator/hooks/useRequestRefreshPreference';
import { useSelfRequestRefresh } from '../../src/self/useSelfRequestRefresh';
import '../../src/styles.css';
import '../../src/theme.css';

function Fixture() {
  const operator = useRequestRefreshPreference();
  const portal = useSelfRequestRefresh(() => undefined, true);
  return <main>
    <div data-refresh-scope="operator">
      <RequestRefreshControl intervalMs={operator.intervalMs} onIntervalChange={operator.onIntervalChange} paused={operator.paused} />
    </div>
    <div data-refresh-scope="portal">
      <RequestRefreshControl intervalMs={portal.intervalMs} onIntervalChange={portal.setIntervalMs} paused={portal.paused} supportsLive={false} />
    </div>
  </main>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
