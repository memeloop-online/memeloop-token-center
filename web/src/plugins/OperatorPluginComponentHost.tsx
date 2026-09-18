import {
  Component,
  Suspense,
  lazy,
  useMemo,
  useState,
  type ComponentType,
  type ErrorInfo,
  type ReactNode,
} from 'react';
import * as ReactRuntime from 'react';
import * as FluentRuntime from '@fluentui/react-components';
import { api } from '../api.js';
import type { PluginOperatorUiContribution, PluginServiceDataResponse } from '../types.js';
import {
  OPERATOR_UI_PACKAGE_API_V1,
  defineOperatorUiPackage,
  operatorUiPackageSupportsManifest,
  type OperatorUiComponentPropsV1,
  type OperatorUiContributionV1,
  type OperatorUiHostApiV1,
  type OperatorUiModuleV1,
  type OperatorUiPackageV1,
} from '../../operator-ui-sdk/index.js';

export interface OperatorPluginComponentHostProps {
  pluginId: string;
  pluginVersion: string;
  contribution: PluginOperatorUiContribution;
  serviceEndpointIds: readonly string[];
  credential: string;
  tenantExternalId: string;
  locale: string;
  compact?: boolean;
  onNavigate: (route: string) => void;
}

interface BoundaryProps {
  children: ReactNode;
  onReset: () => void;
}

class PluginContributionBoundary extends Component<BoundaryProps, { failed: boolean }> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidCatch(_error: Error, _info: ErrorInfo) {
    // Keep one extension failure local to its contribution boundary.
  }

  render() {
    if (!this.state.failed) return this.props.children;
    return <div className="notice error" role="alert">
      <span>This plugin view could not be loaded.</span>
      <button type="button" className="secondary" onClick={this.props.onReset}>Try again</button>
    </div>;
  }
}

const packageLoads = new Map<string, Promise<OperatorUiPackageV1>>();

function serviceDataPath(pluginId: string, endpointId: string, tenant: string) {
  const query = tenant ? `?${new URLSearchParams({ tenant_external_id: tenant }).toString()}` : '';
  return `/internal/v1/plugins/${encodeURIComponent(pluginId)}/data/${encodeURIComponent(endpointId)}${query}`;
}

function moduleUrl(pluginId: string, pluginVersion: string, contribution: PluginOperatorUiContribution): string {
  const entry = contribution.module_entry;
  const digest = contribution.module_sha256;
  if (!entry || !digest) throw new Error('Plugin UI module identity is incomplete');
  const encodedEntry = entry.split('/').map(encodeURIComponent).join('/');
  const expectedPath = `/ui-assets/plugins/${encodeURIComponent(pluginId)}/${encodeURIComponent(pluginVersion)}/${encodeURIComponent(digest)}/${encodedEntry}`;
  const url = new URL(expectedPath, window.location.origin);
  if (url.origin !== window.location.origin || url.pathname !== expectedPath) {
    throw new Error('Plugin UI module URL is invalid');
  }
  return url.href;
}

async function loadPackage(url: string): Promise<OperatorUiPackageV1> {
  const loaded = await import(/* @vite-ignore */ url) as Partial<OperatorUiModuleV1>;
  if (typeof loaded.activateOperatorUi !== 'function') throw new Error('Plugin UI module has no activation export');
  const packageValue = await loaded.activateOperatorUi(Object.freeze({
    apiVersion: OPERATOR_UI_PACKAGE_API_V1,
    React: ReactRuntime,
    Fluent: FluentRuntime,
    defineOperatorUiPackage,
  }));
  if (!packageValue || typeof packageValue !== 'object') throw new Error('Plugin UI module returned an invalid package');
  return packageValue;
}

function componentLoad(
  url: string,
  pluginId: string,
  pluginVersion: string,
  componentId: string,
): Promise<{ default: ComponentType<OperatorUiComponentPropsV1> }> {
  let packageLoad = packageLoads.get(url);
  if (!packageLoad) {
    packageLoad = loadPackage(url);
    packageLoads.set(url, packageLoad);
  }
  return packageLoad.then((packageValue) => {
    if (!operatorUiPackageSupportsManifest(packageValue, pluginId, pluginVersion)) {
      throw new Error('Plugin UI package identity is incompatible with its manifest');
    }
    if (!Object.hasOwn(packageValue.components, componentId)) {
      throw new Error('Plugin UI package does not export the requested component');
    }
    return { default: packageValue.components[componentId]! };
  });
}

function PluginComponent({
  pluginId,
  pluginVersion,
  contribution,
  serviceEndpointIds,
  credential,
  tenantExternalId,
  locale,
  compact,
  onNavigate,
  retry,
}: OperatorPluginComponentHostProps & { compact: boolean; retry: number }) {
  const url = moduleUrl(pluginId, pluginVersion, contribution);
  const LazyComponent = useMemo(
    () => lazy(() => componentLoad(url, pluginId, pluginVersion, contribution.component_id!)),
    [contribution.component_id, pluginId, pluginVersion, retry, url],
  );
  const hostApi = useMemo<OperatorUiHostApiV1>(() => ({
    loadServiceData(endpointId: string, signal?: AbortSignal) {
      if (!serviceEndpointIds.includes(endpointId)) {
        return Promise.reject(new Error(`Unknown service-data endpoint: ${endpointId}`));
      }
      return api<PluginServiceDataResponse>(serviceDataPath(pluginId, endpointId, tenantExternalId), credential, { signal });
    },
    navigate: onNavigate,
  }), [credential, onNavigate, pluginId, serviceEndpointIds, tenantExternalId]);

  return <Suspense fallback={<div className="empty" role="status">Loading plugin view…</div>}>
    <LazyComponent
      pluginId={pluginId}
      pluginVersion={pluginVersion}
      contribution={contribution as OperatorUiContributionV1}
      tenantExternalId={tenantExternalId}
      locale={locale}
      compact={compact}
      api={hostApi}
    />
  </Suspense>;
}

export function OperatorPluginComponentHost(props: OperatorPluginComponentHostProps) {
  const [retry, setRetry] = useState(0);
  return <PluginContributionBoundary
    key={retry}
    onReset={() => {
      try {
        packageLoads.delete(moduleUrl(props.pluginId, props.pluginVersion, props.contribution));
      } catch {
        // A refreshed manifest may fix an incomplete module identity.
      }
      setRetry((value) => value + 1);
    }}
  >
    <PluginComponent {...props} compact={props.compact ?? false} retry={retry} />
  </PluginContributionBoundary>;
}
