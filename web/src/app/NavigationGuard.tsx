import { createContext, useContext, useLayoutEffect, useRef, type ReactNode } from 'react';

type Guard = () => Promise<boolean>;
export interface NavigationGuard {
  register: (guard: Guard) => () => void;
  navigate: (action: () => void) => Promise<boolean>;
}

const fallback: NavigationGuard = {
  register: () => () => {},
  navigate: async action => { action(); return true; },
};

const context = createContext<NavigationGuard | undefined>(undefined);

export function createNavigationGuard(): NavigationGuard {
  const guards = new Set<Guard>();
  let pending = false;
  return {
    register: guard => { guards.add(guard); return () => { guards.delete(guard); }; },
    navigate: async action => {
      if (pending) return false;
      pending = true;
      try {
        for (const guard of guards) if (!await guard()) return false;
        action();
        return true;
      } finally { pending = false; }
    },
  };
}

export function NavigationGuardProvider({ children, controller }: { children: ReactNode; controller?: NavigationGuard }) {
  const inherited = useContext(context);
  const value = useRef<NavigationGuard>(createNavigationGuard());
  return inherited ? children : <context.Provider value={controller ?? value.current}>{children}</context.Provider>;
}

export function useGuardedNavigation() {
  return (useContext(context) ?? fallback).navigate;
}

export function useNavigationGuard(guard: Guard) {
  const navigation = useContext(context) ?? fallback;
  const latest = useRef(guard);
  latest.current = guard;
  useLayoutEffect(() => navigation.register(() => latest.current()), [navigation]);
}
