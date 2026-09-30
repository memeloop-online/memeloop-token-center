const storageKey = 'mtc-codex-device-recovery';

export interface DeviceLoginRecovery {
  session_id: string;
  tenant: string;
  account_id?: string;
  expires_at: number;
}

export function readDeviceLoginRecovery(tenant: string): DeviceLoginRecovery | undefined {
  try {
    const value = JSON.parse(sessionStorage.getItem(storageKey) ?? 'null') as DeviceLoginRecovery | null;
    if (!value || value.tenant !== tenant || !/^[0-9a-f-]{36}$/i.test(value.session_id) || !Number.isFinite(value.expires_at)
      || Date.now() >= value.expires_at + 24 * 60 * 60 * 1000 || (value.account_id !== undefined && typeof value.account_id !== 'string')) return;
    return { session_id: value.session_id, tenant: value.tenant, account_id: value.account_id, expires_at: value.expires_at };
  } catch { return; }
}

export function saveDeviceLoginRecovery(value: DeviceLoginRecovery) {
  try { sessionStorage.setItem(storageKey, JSON.stringify({ session_id: value.session_id, tenant: value.tenant, account_id: value.account_id, expires_at: value.expires_at })); }
  catch { }
}

export function clearDeviceLoginRecovery(sessionId?: string) {
  try {
    const value = JSON.parse(sessionStorage.getItem(storageKey) ?? 'null') as DeviceLoginRecovery | null;
    if (value?.session_id === sessionId) sessionStorage.removeItem(storageKey);
  } catch { }
}
