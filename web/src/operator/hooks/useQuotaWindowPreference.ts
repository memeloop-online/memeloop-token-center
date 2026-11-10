import { useEffect, useState } from 'react';
import type { UpstreamQuotaSnapshot } from '../upstreamQuota';

const changed = 'mtc-quota-display-window-changed';

export function useQuotaWindowPreference(snapshot?: UpstreamQuotaSnapshot) {
  const key = snapshot ? `mtc.quota.display-window.v1:${JSON.stringify([snapshot.tenant_external_id, snapshot.upstream_account_id])}` : '';
  const read = () => {
    try { return key ? window.localStorage.getItem(key) : null; } catch { return null; }
  };
  const [saved, setSaved] = useState(() => ({ key, value: read() }));
  const value = saved.key === key ? saved.value : read();
  useEffect(() => {
    const update = () => setSaved({ key, value: read() });
    update();
    window.addEventListener('storage', update);
    window.addEventListener(changed, update);
    return () => {
      window.removeEventListener('storage', update);
      window.removeEventListener(changed, update);
    };
  }, [key]);
  function select(id: string) {
    if (!key || (id && !snapshot?.windows.some(window => window.id === id))) return;
    setSaved({ key, value: id || null });
    try {
      if (id) window.localStorage.setItem(key, id);
      else window.localStorage.removeItem(key);
      window.dispatchEvent(new Event(changed));
    } catch { /* Selection still works when browser storage is unavailable. */ }
  }
  return { value: snapshot?.windows.some(window => window.id === value) ? value : null, select };
}
