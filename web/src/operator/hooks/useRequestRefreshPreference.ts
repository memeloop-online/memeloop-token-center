import { useEffect, useState } from 'react';
import { defaultRequestRefreshInterval, requestRefreshPreference, requestRefreshPreferenceKey } from '../traffic/requestRefresh.js';

export function useRequestRefreshPreference() {
  const [intervalMs, setInterval] = useState(() => {
    try { return requestRefreshPreference(localStorage.getItem(requestRefreshPreferenceKey)); }
    catch { return defaultRequestRefreshInterval; }
  });
  const [paused, setPaused] = useState(() => document.hidden);
  useEffect(() => {
    const visibility = () => setPaused(document.hidden);
    const storage = (event: StorageEvent) => {
      if (event.key === requestRefreshPreferenceKey) setInterval(requestRefreshPreference(event.newValue));
    };
    document.addEventListener('visibilitychange', visibility);
    window.addEventListener('storage', storage);
    return () => { document.removeEventListener('visibilitychange', visibility); window.removeEventListener('storage', storage); };
  }, []);
  function onIntervalChange(value: number) {
    const next = requestRefreshPreference(String(value));
    setInterval(next);
    try { localStorage.setItem(requestRefreshPreferenceKey, String(next)); } catch { /* Preference remains usable in private storage contexts. */ }
  }
  return { intervalMs, onIntervalChange, paused };
}
