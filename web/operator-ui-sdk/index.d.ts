import type { ComponentType } from 'react';

export declare const OPERATOR_UI_PACKAGE_API_V1: 'operator-ui-package-v1';

export interface OperatorUiContributionV1 {
  id: string;
  slot: 'operator.sidebar.tab' | 'operator.overview.card' | 'operator.page.before' | 'operator.page.after';
  category?: { id: string; label?: string | null } | null;
  route?: string | null;
  target_route?: string | null;
  label: string;
  icon: string;
  renderer: 'component_v1';
  component_id: string;
  component_props?: Record<string, unknown> | null;
  data_endpoint?: string | null;
}

export interface OperatorUiServiceDataResponseV1 {
  data: Record<string, unknown>;
  partial: boolean;
  provenance: {
    plugin_id: string;
    endpoint_id: string;
    origin: string;
    fetched_at: number;
    source: 'network' | 'cache' | 'stale_cache' | 'fallback';
  };
}

export interface OperatorUiRequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
  body?: unknown;
  headers?: Readonly<Record<string, string>>;
  signal?: AbortSignal;
}

export interface OperatorUiHostApiV1 {
  request<T>(path: string, options?: OperatorUiRequestOptions): Promise<T>;
  loadServiceData(endpointId: string, signal?: AbortSignal): Promise<OperatorUiServiceDataResponseV1>;
  navigate(route: string): void;
}

export interface OperatorUiComponentPropsV1 {
  pluginId: string;
  pluginVersion: string;
  contribution: OperatorUiContributionV1;
  tenantExternalId: string;
  locale: string;
  compact: boolean;
  api: OperatorUiHostApiV1;
}

export interface OperatorUiPackageV1 {
  apiVersion: typeof OPERATOR_UI_PACKAGE_API_V1;
  pluginId: string;
  compatiblePluginVersions?: readonly string[];
  components: Readonly<Record<string, ComponentType<OperatorUiComponentPropsV1>>>;
}

export declare function defineOperatorUiPackage(value: OperatorUiPackageV1): OperatorUiPackageV1;
export declare function operatorUiPackageSupportsManifest(value: OperatorUiPackageV1, pluginId: string, pluginVersion: string): boolean;
