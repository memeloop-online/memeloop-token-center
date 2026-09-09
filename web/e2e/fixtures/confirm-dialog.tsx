import { StrictMode, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { useConfirmDialog } from '../../src/useConfirmDialog';
import '../../src/styles.css';
import '../../src/theme.css';

declare global {
  interface Window {
    confirmFixture: {
      accepted: number;
      changeScope: () => void;
      duplicate: () => void;
      stale: () => void;
      unmount: () => void;
    };
  }
}

function Fixture() {
  const [scope, setScope] = useState(0);
  const { confirm, confirmationDialog } = useConfirmDialog([scope]);
  const ask = async () => {
    if (await confirm('Confirm fixture action\nNo live operations are performed.')) window.confirmFixture.accepted += 1;
  };
  window.confirmFixture.changeScope = () => setScope((value) => value + 1);
  window.confirmFixture.duplicate = () => { void ask(); void ask(); };
  if (!window.confirmFixture.stale) window.confirmFixture.stale = () => { void ask(); };
  return <main><button type="button" onClick={() => void ask()}>Open confirmation</button>{confirmationDialog}</main>;
}
const root = createRoot(document.getElementById('root')!);
window.confirmFixture = { accepted: 0, unmount: () => root.unmount() } as Window['confirmFixture'];
root.render(<StrictMode><I18nProvider><Fixture /></I18nProvider></StrictMode>);
