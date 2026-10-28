import { api } from '../api.js';
import type { RequestConfigSnapshot } from './requestConfig.js';

/**
 * Single wiring point for the request-configuration settings API. The backend
 * settings factory owns the final contract (http-settings-20261028); only this
 * module changes when the typed contract lands.
 */
export const requestConfigEndpoint = '/internal/v1/request-config/settings';
export const requestConfigContractVersion = 'request_config_v1';

export type RequestConfigScope = { scope: 'default' } | { scope: 'purpose'; purpose: string };

function checked(snapshot: RequestConfigSnapshot): RequestConfigSnapshot {
  if (!snapshot || snapshot.contract_version !== requestConfigContractVersion
    || typeof snapshot.defaults !== 'object' || typeof snapshot.purposes !== 'object') {
    throw new Error('Unexpected request-config settings contract');
  }
  return snapshot;
}

export async function loadRequestConfig(token: string, tenant: string): Promise<RequestConfigSnapshot> {
  return checked(await api<RequestConfigSnapshot>(`${requestConfigEndpoint}?tenant_external_id=${encodeURIComponent(tenant)}`, token));
}

/** A null value clears that field's override (purpose scope) or platform limit (defaults). */
export async function saveRequestConfig(
  token: string,
  tenant: string,
  scope: RequestConfigScope,
  values: Record<string, number | null>,
  expectedUpdatedAt: number | null,
): Promise<RequestConfigSnapshot> {
  return checked(await api<RequestConfigSnapshot>(requestConfigEndpoint, token, {
    method: 'PUT',
    body: JSON.stringify({ tenant_external_id: tenant, ...scope, values, expected_updated_at: expectedUpdatedAt }),
  }));
}
