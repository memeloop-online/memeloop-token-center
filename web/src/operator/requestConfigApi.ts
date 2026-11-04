import { api } from '../api.js';
import { requestConfigFields, type HttpOverrides, type HttpSettings, type RequestConfigPurpose, type RequestConfigSnapshot } from './requestConfig.js';

export const requestConfigEndpoint = '/internal/v1/system-settings/http';
export type RequestConfigScope = { scope: 'default' } | { scope: 'purpose'; purpose: RequestConfigPurpose };

export async function loadRequestConfig(token: string): Promise<RequestConfigSnapshot> {
  return api<RequestConfigSnapshot>(requestConfigEndpoint, token);
}

function replaceKnownOverrides(current: HttpOverrides, values: HttpOverrides): HttpOverrides {
  const next = { ...current } as Record<string, unknown>;
  for (const field of requestConfigFields) {
    if (Object.prototype.hasOwnProperty.call(values, field.key)) next[field.key] = values[field.key];
    else delete next[field.key];
  }
  return next as HttpOverrides;
}

export async function saveRequestConfig(token: string, scope: RequestConfigScope, values: HttpOverrides, snapshot: RequestConfigSnapshot, clearPurpose = false): Promise<RequestConfigSnapshot> {
  const settings: HttpSettings = {
    defaults: scope.scope === 'default' ? replaceKnownOverrides(snapshot.settings.defaults, values) : { ...snapshot.settings.defaults },
    purposes: { ...snapshot.settings.purposes },
  };
  if (scope.scope === 'purpose') {
    if (clearPurpose) delete settings.purposes[scope.purpose];
    else {
      const next = replaceKnownOverrides(snapshot.settings.purposes[scope.purpose] ?? {}, values);
      if (Object.keys(next).length) settings.purposes[scope.purpose] = next;
      else delete settings.purposes[scope.purpose];
    }
  }
  return api<RequestConfigSnapshot>(requestConfigEndpoint, token, {
    method: 'PUT',
    body: JSON.stringify({ expected_revision: snapshot.revision, settings }),
  });
}
