import { lazy, StrictMode, Suspense } from 'react';
import { createRoot } from 'react-dom/client';
import { RouteErrorBoundary } from '../../src/app/RouteErrorBoundary';

const RejectedRoute = lazy(async () => {
  if (sessionStorage.getItem('route-error-fixture-rejected') !== 'true') {
    sessionStorage.setItem('route-error-fixture-rejected', 'true');
    throw new Error('simulated lazy route rejection');
  }
  return { default: () => <h1>Recovered route module</h1> };
});

createRoot(document.getElementById('root')!).render(<StrictMode>
  <RouteErrorBoundary
    copy={{
      eyebrow: 'Page load failed',
      title: 'This page cannot be displayed',
      description: 'A page module did not finish loading.',
      retry: 'Try again',
      refresh: 'Refresh page',
    }}
    resetKey="operator:usage"
  >
    <Suspense fallback={<p>Loading route</p>}><RejectedRoute /></Suspense>
  </RouteErrorBoundary>
</StrictMode>);
