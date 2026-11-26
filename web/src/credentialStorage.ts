export type RememberedCredentialKind = 'operator' | 'self';

const storageKeys: Record<RememberedCredentialKind, string> = {
  operator: 'mtc.operator.service-credential.v1',
  self: 'mtc.self.client-credential.v1',
};
export const operatorCredentialStorageKey = storageKeys.operator;
// This random, non-secret identity marks a remembered login, never its credential.
export const operatorLoginCycleStorageKey = 'mtc.operator.login-cycle.v1';

export function readOperatorLoginCycle(storage = browserStorage()): string | undefined {
  try { return storage?.getItem(operatorLoginCycleStorageKey) || undefined; } catch { return undefined; }
}

export function isOperatorLoginCycleCurrent(cycle: string): boolean {
  return Boolean(cycle) && readOperatorLoginCycle() === cycle;
}

export function ensureOperatorLoginCycle(storage = browserStorage()): string | undefined {
  if (!storage) return undefined;
  try {
    const existing = readOperatorLoginCycle(storage);
    if (existing) return existing;
    const cycle = crypto.randomUUID();
    storage.setItem(operatorLoginCycleStorageKey, cycle);
    return cycle;
  } catch { return undefined; }
}

export function clearOperatorLoginCycle(storage = browserStorage()): void {
  try { storage?.removeItem(operatorLoginCycleStorageKey); } catch { /* Storage can be unavailable. */ }
}

const operatorTenantStorageKey = 'mtc.operator.tenant.v1';

function browserStorage(): Storage | undefined {
  if (typeof window === 'undefined') return undefined;
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}

export function readRememberedCredential(kind: RememberedCredentialKind, storage = browserStorage()): string {
  if (!storage) return '';
  try {
    return storage.getItem(storageKeys[kind])?.trim() ?? '';
  } catch {
    return '';
  }
}

export function rememberCredential(kind: RememberedCredentialKind, credential: string, storage = browserStorage()): void {
  if (!storage) return;
  const value = credential.trim();
  if (!value) return;
  try {
    if (kind === 'operator' && readRememberedCredential(kind, storage) !== value) clearOperatorLoginCycle(storage);
    storage.setItem(storageKeys[kind], value);
  } catch {
    // A blocked or full browser store must not prevent an explicit login.
  }
}

export function clearRememberedCredential(kind: RememberedCredentialKind, storage = browserStorage()): void {
  if (!storage) return;
  try {
    if (kind === 'operator') clearOperatorLoginCycle(storage);
    storage.removeItem(storageKeys[kind]);
  } catch {
    // Clearing in-memory state still signs this page out when storage is blocked.
  }
}

export function readRememberedOperatorTenant(storage = browserStorage()): string {
  if (!storage) return '';
  try {
    return storage.getItem(operatorTenantStorageKey)?.trim() ?? '';
  } catch {
    return '';
  }
}

export function rememberOperatorTenant(tenant: string, storage = browserStorage()): void {
  if (!storage) return;
  try {
    const value = tenant.trim();
    if (value) storage.setItem(operatorTenantStorageKey, value);
    else storage.removeItem(operatorTenantStorageKey);
  } catch {
    // Tenant selection is an ergonomic preference, never an auth dependency.
  }
}

export function tenantForCredential(
  tenants: ReadonlyArray<{ external_id: string }>,
  previousTenant: string,
): string {
  if (tenants.some((value) => value.external_id === previousTenant)) return previousTenant;
  if (tenants.some((value) => value.external_id === 'default')) return 'default';
  return tenants[0]?.external_id ?? '';
}
