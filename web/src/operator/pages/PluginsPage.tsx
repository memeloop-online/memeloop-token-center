import { api } from '../../api';
import { useI18n } from '../../i18n';
import type { PluginManifest } from '../../types';
import { PluginRuntimeManager } from '../PluginRuntimeManager';
import { Plugins } from '../Plugins';
import { useOperatorResource } from '../hooks/useOperatorResource';
import type { ResourceState } from '../hooks/useOperatorResource';

interface PluginsPageProps {
  token: string;
  /** Selected read scope; empty only when no tenant is available. */
  tenant: string;
  /** Explicit target for any mutation rendered by the page. */
  writeTenant?: string;
  catalog: ResourceState<PluginManifest[]>;
  reloadCatalog: () => Promise<void>;
}

export function PluginsPage({ token, tenant, writeTenant, catalog, reloadCatalog }: PluginsPageProps) {
  const { t } = useI18n();
  // Selected tenant is a read filter, not the authenticated principal's scope.
  // Ask only for self capabilities before mounting any global data consumer.
  const access = useOperatorResource(
    Boolean(token), token,
    (signal) => api<{ can_view_runtime: boolean; can_manage_runtime: boolean }>('/internal/v1/plugins/runtime-access', token, { signal }),
    t('common.requestFailed'),
  );
  const manager = access.state.scopeKey !== token ? null : access.state.kind === 'failed'
    ? <p className="notice error" role="alert">{access.state.message}</p>
    : access.state.kind === 'ready' && access.state.value.can_view_runtime
      ? <PluginRuntimeManager key={token} token={token} canManage={access.state.value.can_manage_runtime} onPublished={reloadCatalog} /> : null;
  if (catalog.scopeKey !== token || catalog.kind === 'idle' || catalog.kind === 'loading') return <>{manager}<div className="empty">{t('common.loading')}</div></>;
  if (catalog.kind === 'failed') return <>{manager}<div className="notice error" role="alert">{catalog.message}<button type="button" onClick={() => void reloadCatalog()}>{t('common.retry')}</button></div></>;
  return <>{manager}<button type="button" className="secondary" onClick={() => void reloadCatalog()}>{t('plugins.refreshCatalog')}</button>{catalog.refreshError && <div className="notice error" role="alert">{catalog.refreshError}</div>}<Plugins key={`${token}\0${tenant}\0${writeTenant}`} token={token} tenant={tenant} writeTenant={writeTenant} values={catalog.value} /></>;
}
