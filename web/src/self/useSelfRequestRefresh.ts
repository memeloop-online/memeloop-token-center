import { useCallback, useEffect, useRef, useState } from 'react';
import {
  defaultRequestRefreshInterval,
  requestRefreshIntervals,
  requestRefreshPreference,
  selfRequestRefreshPreferenceKey,
} from '../operator/traffic/requestRefresh';

const selfRequestRefreshPreferenceEvent = 'mtc-self-request-refresh-preference';

/**
 * Same cadence ladder as operator traffic, but self requests are GET polling
 * only: there is no self-service event stream, so 0 means manual refresh and
 * must never be presented as live.
 */
export const selfRequestRefreshIntervals = requestRefreshIntervals;
export const defaultSelfRequestRefreshInterval: number = defaultRequestRefreshInterval;

export interface SelfRequestRefresh {
  intervalMs: number;
  paused: boolean;
  setIntervalMs: (value: number) => void;
}

/**
 * Local polling cadence for the self-service requests page. The callback owns
 * all credentials and fetching; this hook only schedules ticks for a nonzero
 * interval once initial data exists. Requests and Sessions share this Portal
 * preference, while the Operator traffic view keeps its own live-stream setting.
 */
export function useSelfRequestRefresh(refresh: () => void, ready: boolean): SelfRequestRefresh {
  const [intervalMs, setIntervalMsState] = useState<number>(() => {
    try {
      return requestRefreshPreference(window.localStorage.getItem(selfRequestRefreshPreferenceKey));
    } catch {
      return defaultSelfRequestRefreshInterval;
    }
  });
  const [paused, setPaused] = useState<boolean>(() => document.hidden);
  const refreshRef = useRef(refresh);
  refreshRef.current = refresh;

  const setIntervalMs = useCallback((value: number) => {
    const next = selfRequestRefreshIntervals.some((interval) => interval === value) ? value : defaultSelfRequestRefreshInterval;
    setIntervalMsState(next);
    try {
      window.localStorage.setItem(selfRequestRefreshPreferenceKey, String(next));
      window.dispatchEvent(new CustomEvent(selfRequestRefreshPreferenceEvent, { detail: next }));
    } catch {
      // Keep the in-memory selection usable in private storage contexts.
    }
  }, []);

  useEffect(() => {
    const onStorage = (event: StorageEvent) => {
      if (event.key === selfRequestRefreshPreferenceKey) setIntervalMsState(requestRefreshPreference(event.newValue));
    };
    const onPreference = (event: Event) => {
      const value = (event as CustomEvent<number>).detail;
      setIntervalMsState(requestRefreshPreference(String(value)));
    };
    window.addEventListener('storage', onStorage);
    window.addEventListener(selfRequestRefreshPreferenceEvent, onPreference);
    return () => {
      window.removeEventListener('storage', onStorage);
      window.removeEventListener(selfRequestRefreshPreferenceEvent, onPreference);
    };
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
