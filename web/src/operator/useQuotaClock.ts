import { useSyncExternalStore } from 'react';

const listeners = new Set<() => void>();
let now = Date.now();
let timer: ReturnType<typeof setInterval> | undefined;
function subscribe(listener: () => void) {
  listeners.add(listener);
  if (!timer) {
    now = Date.now();
    timer = setInterval(() => {
      now = Date.now();
      listeners.forEach(notify => notify());
    }, 1_000);
  }
  return () => {
    listeners.delete(listener);
    if (!listeners.size && timer) { clearInterval(timer); timer = undefined; }
  };
}

/** One clock per UI, not one interval per account or quota window. No network reads. */
export function useQuotaClock() {
  return useSyncExternalStore(subscribe, () => now, () => now);
}
