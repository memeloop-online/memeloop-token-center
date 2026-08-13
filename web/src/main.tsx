import { lazy, StrictMode, Suspense } from 'react';
import { createRoot } from 'react-dom/client';
import { SelfPortal } from './self/SelfPortal';
import { I18nProvider } from './i18n';
import './styles.css';
import './theme.css';

const isOperator = window.location.pathname.startsWith('/operator');
const Operator = lazy(() => import('./operator/Operator').then((module) => ({ default: module.Operator })));

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <I18nProvider>
      {isOperator ? <Suspense fallback={<div className="boot">Loading…</div>}><Operator /></Suspense> : <SelfPortal />}
    </I18nProvider>
  </StrictMode>,
);
