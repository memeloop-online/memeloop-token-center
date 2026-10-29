import { api } from '../api.js';
import type { HttpOverrides, HttpSettings, RequestConfigPurpose, RequestConfigSnapshot } from './requestConfig.js';

export const requestConfigEndpoint = '/internal/v1/system-settings/http';
export type RequestConfigScope = { scope: 'default' } | { scope: 'purpose'; purpose: RequestConfigPurpose };

export async function loadRequestConfig(token: string): Promise<RequestConfigSnapshot> {
  return api<RequestConfigSnapshot>(requestConfigEndpoint, token);
}

export async function saveRequestConfig(token: string, scope: RequestConfigScope, values: HttpOverrides, snapshot: RequestConfigSnapshot): Promise<RequestConfigSnapshot> {
  const settings: HttpSettings = {
    defaults: scope.scope === 'default' ? values : { ...snapshot.settings.defaults },
    purposes: { ...snapshot.settings.purposes },
  };
  if (scope.scope === 'purpose') {
    if (Object.keys(values).length) settings.purposes[scope.purpose] = values;
    else delete settings.purposes[scope.purpose];
  }
  return api<RequestConfigSnapshot>(requestConfigEndpoint, token, {
    method: 'PUT',
    body: JSON.stringify({ expected_revision: snapshot.revision, settings }),
  });
}
