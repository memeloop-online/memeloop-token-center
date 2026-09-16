import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * Same cadence ladder as operator traffic, but self requests are GET polling
 * only: there is no self-service event stream, so 0 means manual refresh and
 * must never be presented as live.
 */
export const selfRequestRefreshIntervals = [0, 5_000, 30_000, 60_000, 300_000] as const;
export const defaultSelfRequestRefreshInterval: number = 5_000;

export interface SelfRequestRefresh {
  intervalMs: number;
  paused: boolean;
  setIntervalMs: (value: number) => void;
}

/**
 * Local polling cadence for the self-service requests page. The callback owns
 * all credentials and fetching; this hook only schedules ticks for a nonzero
 * interval once initial data exists. The selection lives in component state
 * only: it never touches the operator preference or any localStorage key.
 */
export function useSelfRequestRefresh(refresh: () => void, ready: boolean): SelfRequestRefresh {
  const [intervalMs, setIntervalMsState] = useState<number>(defaultSelfRequestRefreshInterval);
  const [paused, setPaused] = useState<boolean>(() => document.hidden);
  const refreshRef = useRef(refresh);
  refreshRef.current = refresh;

  const setIntervalMs = useCallback((value: number) => {
    setIntervalMsState(
      selfRequestRefreshIntervals.some((interval) => interval === value) ? value : defaultSelfRequestRefreshInterval,
    );
  }, []);

  useEffect(() => {
    const onVisibilityChange = () => setPaused(document.hidden);
    onVisibilityChange();
    document.addEventListener('visibilitychange', onVisibilityChange);
    return () => document.removeEventListener('visibilitychange', onVisibilityChange);
  }, []);

  useEffect(() => {
    if (!intervalMs || paused || !ready) return undefined;
    const timer = window.setInterval(() => refreshRef.current(), intervalMs);
    return () => window.clearInterval(timer);
  }, [intervalMs, paused, ready]);

  return { intervalMs, paused, setIntervalMs };
}
