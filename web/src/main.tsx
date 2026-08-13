import { lazy, StrictMode, Suspense } from 'react';
import { createRoot } from 'react-dom/client';
import { SelfPortal } from './self/SelfPortal';
import './styles.css';
import './theme.css';

const isOperator = window.location.pathname.startsWith('/operator');
const Operator = lazy(() => import('./operator/Operator').then((module) => ({ default: module.Operator })));

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    {isOperator ? <Suspense fallback={<div className="boot">正在加载控制面…</div>}><Operator /></Suspense> : <SelfPortal />}
  </StrictMode>,
);
