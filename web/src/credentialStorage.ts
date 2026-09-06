export type RememberedCredentialKind = 'operator' | 'self';

const storageKeys: Record<RememberedCredentialKind, string> = {
  operator: 'mtc.operator.service-credential.v1',
  self: 'mtc.self.client-credential.v1',
};
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
    storage.setItem(storageKeys[kind], value);
  } catch {
    // A blocked or full browser store must not prevent an explicit login.
  }
}

export function clearRememberedCredential(kind: RememberedCredentialKind, storage = browserStorage()): void {
  if (!storage) return;
  try {
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
  return tenants.length === 1 ? tenants[0]!.external_id : '';
}
