import { useCallback, useEffect, useRef, useState } from 'react';
import { createNavigationGuard } from './NavigationGuard';
import type { ProxyGroupNavigationContext } from './proxyGroupNavigation';
import {
  appHref,
  isSameAppLocation,
  readAppLocation,
  type AppLocation,
  type AppRouteKey,
} from './routes';

function currentLocation(): AppLocation {
  return readAppLocation(new URL(window.location.href));
}

function historyIndex(state: unknown): number | undefined {
  if (typeof state !== 'object' || state === null || !('mtcNavigationIndex' in state)) return;
  return typeof state.mtcNavigationIndex === 'number' && Number.isSafeInteger(state.mtcNavigationIndex) ? state.mtcNavigationIndex : undefined;
}

function historyState(index: number) {
  const previous: unknown = window.history.state;
  return { ...(typeof previous === 'object' && previous !== null ? previous : {}), mtcNavigationIndex: index };
}

export function useAppLocation() {
  const [location, setLocation] = useState(currentLocation);
  const current = useRef(location);
  const index = useRef(historyIndex(window.history.state) ?? 0);
  const guard = useRef(createNavigationGuard());
  const restoring = useRef<number | undefined>(undefined);
  const handling = useRef(false);
  const revision = useRef(0);

  useEffect(() => {
    const canonicalHref = appHref(current.current.surface, current.current.route, current.current.context);
    window.history.replaceState(historyState(index.current), '', canonicalHref);
  }, []);

  useEffect(() => {
    const restore = (targetIndex: number | undefined) => {
      if (targetIndex !== undefined && targetIndex !== index.current) {
        restoring.current = index.current;
        window.history.go(index.current - targetIndex);
      } else window.history.replaceState(historyState(index.current), '', appHref(current.current.surface, current.current.route, current.current.context));
    };
    const onPopState = () => {
      const next = currentLocation();
      const targetIndex = historyIndex(window.history.state);
      if (restoring.current !== undefined) {
        if (targetIndex === restoring.current) restoring.current = undefined;
        else restore(targetIndex);
        return;
      }
      if (handling.current) { revision.current += 1; restore(targetIndex); return; }
      if (isSameAppLocation(current.current, next)) { if (targetIndex !== undefined) index.current = targetIndex; return; }
      const request = ++revision.current;
      handling.current = true;
      void guard.current.navigate(() => {
        if (request !== revision.current) return;
        index.current = targetIndex ?? index.current;
        current.current = next;
        setLocation(next);
      }).then(accepted => {
        if (!accepted && request === revision.current) restore(targetIndex);
      }).finally(() => { handling.current = false; });
    };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  const navigate = useCallback((route: AppRouteKey, replaceOrContext: boolean | ProxyGroupNavigationContext = false) => {
    const context = typeof replaceOrContext === 'boolean' ? undefined : replaceOrContext;
    const next: AppLocation = { surface: current.current.surface, route, ...(route === 'proxy-groups' && context ? { context } : {}) };
    if (isSameAppLocation(current.current, next)) return;
    revision.current += 1;
    const replace = replaceOrContext === true;
    const nextIndex = replace ? index.current : index.current + 1;
    window.history[replace ? 'replaceState' : 'pushState'](historyState(nextIndex), '', appHref(next.surface, route, context));
    index.current = nextIndex;
    current.current = next;
    setLocation(next);
  }, []);

  return { ...location, navigate: Object.assign(navigate, { navigationGuard: guard.current }) };
}
