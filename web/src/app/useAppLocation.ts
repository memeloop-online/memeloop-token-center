import { useCallback, useEffect, useState } from 'react';
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

export function useAppLocation() {
  const [location, setLocation] = useState(currentLocation);

  useEffect(() => {
    const canonicalHref = appHref(location.surface, location.route);
    if (`${window.location.pathname}${window.location.search}` !== canonicalHref) {
      window.history.replaceState(null, '', canonicalHref);
    }
  }, []);

  useEffect(() => {
    const onPopState = () => setLocation((current) => {
      const next = currentLocation();
      return isSameAppLocation(current, next) ? current : next;
    });
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  const navigate = useCallback((route: AppRouteKey, replace = false) => {
    setLocation((current) => {
      const next = { surface: current.surface, route } satisfies AppLocation;
      if (isSameAppLocation(current, next)) return current;
      const href = appHref(current.surface, route);
      window.history[replace ? 'replaceState' : 'pushState'](null, '', href);
      return next;
    });
  }, []);

  return { ...location, navigate };
}
