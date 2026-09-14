import { useEffect, useState } from 'react';
import { apiRead } from '../api';
import { appHref } from '../app/routes';
import { Disclosure } from '../design-system';
import { useI18n } from '../i18n';
import type { GroupView, ModelRouteView, ProviderType, UpstreamAccount } from '../types';
import { CredentialAuthorizationFields } from './CredentialAuthorizationFields';
import { credentialRouteOptions } from './credentialRouteOptions';

export function CredentialRouteAuthorization({ token, tenant, routes, groups, routeIds, groupIds, onRoutes, onGroups }: {
  token: string; tenant: string; routes: ModelRouteView[]; groups: GroupView[];
  routeIds: string[]; groupIds: string[]; onRoutes: (ids: string[]) => void; onGroups: (ids: string[]) => void;
}) {
  const { locale } = useI18n();
  const zh = locale.startsWith('zh');
  const [catalog, setCatalog] = useState<{ token: string; tenant: string; accounts: UpstreamAccount[]; providers: ProviderType[]; failed: boolean }>();
  useEffect(() => {
    const controller = new AbortController();
    if (!token || !tenant) return () => controller.abort();
    void Promise.all([
      apiRead<UpstreamAccount[]>(`/internal/v1/upstreams?${new URLSearchParams({ tenant_external_id: tenant })}`, token, { signal: controller.signal }),
      apiRead<ProviderType[]>('/internal/v1/provider-types', token, { signal: controller.signal }),
    ]).then(([accounts, providers]) => {
      if (!controller.signal.aborted) setCatalog({ token, tenant, accounts, providers, failed: false });
    }).catch(() => {
      if (!controller.signal.aborted) setCatalog({ token, tenant, accounts: [], providers: [], failed: true });
    });
    return () => controller.abort();
  }, [token, tenant]);
  const current = catalog?.token === token && catalog.tenant === tenant ? catalog : undefined;
  const options = credentialRouteOptions(routes, current?.accounts ?? [], current?.providers ?? [], locale);
  const selectedGroups = groupIds.map(id => groups.find(group => group.id === id));
  const effectiveIds = [...new Set([...routeIds, ...selectedGroups.flatMap(group => group?.member_ids ?? [])])];
  const describe = (id: string) => options.find(option => option.value === id);
  return <>
    <p className="field-hint">{zh ? '授权覆盖整条路由的所有候选账号。只限某一账号的特定模型，请选择该账号的专用路由；路由组可复用这些路由。' : 'A grant covers every candidate account in a route. To restrict a model to one account, choose its dedicated route; route groups can reuse those routes.'} <a href={appHref('operator', 'routes')} target="_blank" rel="noopener noreferrer">{zh ? '新建或查看专用路由（新标签页）' : 'Create or inspect a dedicated route (new tab)'}</a></p>
    {!current ? <small role="status">{zh ? '正在读取账号目录…' : 'Loading account catalog…'}</small> : current.failed && <small role="status">{zh ? '账号目录读取失败；保留账号 ID，不代表无账号。' : 'Account catalog could not be read; IDs remain visible. This does not mean accounts are absent.'}</small>}
    <CredentialAuthorizationFields routes={options} groups={groups.map(group => ({ value: group.id, label: group.name, description: `${group.member_ids.length} ${zh ? '条路由' : 'routes'}` }))} routeIds={routeIds} groupIds={groupIds} onRoutes={onRoutes} onGroups={onGroups} />
    <section style={{ overflowWrap: 'anywhere' }} aria-label={zh ? '授权范围预览' : 'Authorization scope preview'}>
      <h4>{zh ? '授权范围预览' : 'Authorization scope preview'}</h4>
      <p className="field-hint">{zh ? '下列为当前配置，不是连通性检查。共享路由或组成员后续变更会改变授权范围；此处不会修改它们。' : 'This is current configuration, not a connectivity check. Later changes to shared routes or group membership change the scope; this editor does not modify them.'}</p>
      {selectedGroups.map((group, index) => <Disclosure key={groupIds[index]} title={`${group?.name ?? groupIds[index]} · ${group ? `${group.member_ids.length} ${zh ? '条路由' : 'routes'}` : (zh ? '组目录未知' : 'Group catalog unknown')}`}><ul>{group?.member_ids.map(id => <li key={id}><span>{describe(id)?.label ?? id}</span><p className="field-hint">{describe(id)?.description ?? (zh ? '路由目录未知' : 'Route catalog unknown')}</p></li>)}</ul></Disclosure>)}
      <ul>{effectiveIds.map(id => <li key={id}><span>{describe(id)?.label ?? id}</span><p className="field-hint">{describe(id)?.description ?? (zh ? '路由目录未知；保留已有授权' : 'Route catalog unknown; existing grant retained')}</p></li>)}</ul>
      {!effectiveIds.length && <small>{zh ? '尚无已知路由授权' : 'No known route grants yet'}</small>}
    </section>
  </>;
}
