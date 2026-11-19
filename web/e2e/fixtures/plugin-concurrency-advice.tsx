import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { PluginConcurrencyAdvice } from '../../src/operator/PluginConcurrencyAdvice';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

function Fixture() {
  const [tenant, setTenant] = useState('tenant-a');
  return <main className="main"><h1>Private observation fixture</h1>
    <button onClick={() => setTenant('tenant-b')}>Switch tenant</button>
    <PluginConcurrencyAdvice token="fixture-only" tenant={tenant} locale={new URLSearchParams(location.search).get('locale') ?? 'zh-CN'} />
  </main>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
