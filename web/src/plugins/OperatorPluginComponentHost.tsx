import { useMemo, type ComponentType } from 'react';
import { api } from '../api.js';
import type { PluginOperatorUiContribution, PluginServiceDataResponse } from '../types.js';
import type {
  OperatorUiComponentPropsV1,
  OperatorUiContributionV1,
  OperatorUiHostApiV1,
} from '../../operator-ui-sdk/index.js';

export interface OperatorPluginComponentHostProps {
  pluginId: string;
  pluginVersion: string;
  contribution: PluginOperatorUiContribution;
  component: ComponentType<OperatorUiComponentPropsV1>;
  serviceEndpointIds: readonly string[];
  credential: string;
  tenantExternalId: string;
  locale: string;
  compact?: boolean;
  onNavigate: (route: string) => void;
}

function serviceDataPath(pluginId: string, endpointId: string, tenant: string) {
  const query = tenant ? `?${new URLSearchParams({ tenant_external_id: tenant }).toString()}` : '';
  return `/internal/v1/plugins/${encodeURIComponent(pluginId)}/data/${encodeURIComponent(endpointId)}${query}`;
}

function isSameOriginPath(path: string): boolean {
  return path.startsWith('/') && !path.startsWith('//');
}

export function OperatorPluginComponentHost({
  pluginId,
  pluginVersion,
  contribution,
  component: Component,
  serviceEndpointIds,
  credential,
  tenantExternalId,
  locale,
  compact = false,
  onNavigate,
}: OperatorPluginComponentHostProps) {
  const hostApi = useMemo<OperatorUiHostApiV1>(() => ({
    async request<T>(path: string, options = {}) {
      if (!isSameOriginPath(path)) throw new Error('Operator API paths use a same-origin absolute path');
      const { body, method = 'GET', headers, signal } = options;
      return api<T>(path, credential, {
        method,
        headers,
        signal,
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
    },
    loadServiceData(endpointId: string, signal?: AbortSignal) {
      if (!serviceEndpointIds.includes(endpointId)) {
        return Promise.reject(new Error(`Unknown service-data endpoint: ${endpointId}`));
      }
      return api<PluginServiceDataResponse>(serviceDataPath(pluginId, endpointId, tenantExternalId), credential, { signal });
    },
    navigate: onNavigate,
  }), [credential, onNavigate, pluginId, serviceEndpointIds, tenantExternalId]);

  return <Component
    pluginId={pluginId}
    pluginVersion={pluginVersion}
    contribution={contribution as OperatorUiContributionV1}
    tenantExternalId={tenantExternalId}
    locale={locale}
    compact={compact}
    api={hostApi}
  />;
}
