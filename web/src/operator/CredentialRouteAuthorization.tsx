import { useEffect, useState } from 'react';
import { apiRead } from '../api';
import { appHref } from '../app/routes';
import { Button, DetailTooltip } from '../design-system';
import { useI18n } from '../i18n';
import type { GroupView, ModelRouteView, ProviderType, UpstreamAccount } from '../types';
import { CredentialAuthorizationFields } from './CredentialAuthorizationFields';
import { credentialRouteOptions } from './credentialRouteOptions';
import './credentialRoutePresentation.css';

export function CredentialRouteAuthorization({ token, tenant, routes, groups, routeIds, groupIds, onRoutes, onGroups }: {
  token: string; tenant: string; routes: ModelRouteView[]; groups: GroupView[];
  routeIds: string[]; groupIds: string[]; onRoutes: (ids: string[]) => void; onGroups: (ids: string[]) => void;
}) {
  const { locale } = useI18n();
  const zh = locale.startsWith('zh');
  const [retry, setRetry] = useState(0);
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
  }, [token, tenant, retry]);
  const current = catalog?.token === token && catalog.tenant === tenant ? catalog : undefined;
  const options = credentialRouteOptions(routes, current?.accounts ?? [], current?.providers ?? [], locale);
  const selectedGroups = groupIds.map(id => groups.find(group => group.id === id));
  const effectiveIds = [...new Set([...routeIds, ...selectedGroups.flatMap(group => group?.member_ids ?? [])])];
  const describe = (id: string) => options.find(option => option.value === id);
  // Grant-source identity is internal and stable: a direct sentinel plus real
  // group IDs, never translated labels or names, so same-name groups stay
  // distinct. Grouping also keys on the exact account ID set, candidate catalog
  // state, and availability; missing routes key on their own route ID so
  // unavailable grants can never merge accidentally.
  const sourceIdentities = (id: string) => [
    ...(routeIds.includes(id) ? ['direct'] : []),
    ...selectedGroups.filter(group => group?.member_ids.includes(id)).map(group => group!.id).sort(),
  ];
  const sourceNames = (id: string) => [
    ...(routeIds.includes(id) ? [zh ? '直接授权' : 'Direct grant'] : []),
    ...selectedGroups.filter(group => group?.member_ids.includes(id)).map(group => group!.name),
  ];
  const previewGroups = effectiveIds.map(id => {
    const option = describe(id);
    const key = option
      ? JSON.stringify(['scope', option.accountIds, option.catalogState, option.availability, sourceIdentities(id)])
      : JSON.stringify(['missing', id, sourceIdentities(id)]);
    return { id, key };
  }).reduce<Array<{ key: string; ids: string[] }>>((merged, item) => {
    const existing = merged.find(group => group.key === item.key);
    if (existing) existing.ids.push(item.id);
    else merged.push({ key: item.key, ids: [item.id] });
    return merged;
  }, []);
  return <>
    <p className="field-hint">{zh ? '只授权某一账号，请选择该账号的专用路由。' : 'To authorize one account, choose its dedicated route.'} <a href={appHref('operator', 'routes')} target="_blank" rel="noopener noreferrer">{zh ? '新建或查看专用路由（新标签页）' : 'Create or inspect a dedicated route (new tab)'}</a> <DetailTooltip content={zh ? '授权包含整条路由的候选账号。路由组可复用这些路由，后续修改共享路由或组成员也会改变授权范围。' : 'Grants cover every candidate account in a route. Groups reuse these routes; later shared-route or group changes also change the scope.'}><Button appearance="subtle" type="button">{zh ? '授权范围说明' : 'About grant scope'}</Button></DetailTooltip></p>
    {!current ? <small role="status">{zh ? '正在读取账号目录…' : 'Loading account catalog…'}</small> : current.failed && <div><small role="status">{zh ? '账号目录读取失败' : 'Account catalog could not be read'}</small> <Button appearance="subtle" type="button" onClick={() => { setCatalog(undefined); setRetry(value => value + 1); }}>{zh ? '重试读取目录' : 'Retry account catalog'}</Button></div>}
    <CredentialAuthorizationFields routes={options} groups={groups.map(group => ({ value: group.id, label: group.name, description: `${group.member_ids.length} ${zh ? '条路由' : 'routes'}` }))} routeIds={routeIds} groupIds={groupIds} onRoutes={onRoutes} onGroups={onGroups} />
    <section className="credential-route-preview" aria-label={zh ? '授权范围预览' : 'Authorization scope preview'}>
      <h4>{zh ? '授权范围预览' : 'Authorization scope preview'}</h4>
      {selectedGroups.some(group => !group) && <small>{zh ? '部分路由组目录不可用' : 'Some route groups are unavailable in the catalog'}</small>}
      <ul>{previewGroups.map(group => {
        const option = describe(group.ids[0]);
        return <li key={group.key}>
          <p className="field-hint">{option?.scope ?? (zh ? '路由目录未知' : 'Route catalog unknown')}</p>
          {option?.accountDescription && <p className="field-hint">{zh ? '账号' : 'Accounts'}: {option.accountDescription}</p>}
          <p className="field-hint">{option?.disabled && `${zh ? '当前不可用' : 'Currently unavailable'} · `}{zh ? '授权来源' : 'Grant source'}: {sourceNames(group.ids[0]).join(' / ')}</p>
          <div className="credential-route-models">{group.ids.map(id => {
            const model = describe(id);
            return <DetailTooltip key={id} content={model?.details ?? id}><span tabIndex={0} className="credential-route-model">{model?.label ?? `${zh ? '未知路由' : 'Unknown route'} …${id.slice(-6)}`}</span></DetailTooltip>;
          })}</div>
        </li>;
      })}</ul>
      {!effectiveIds.length && <small>{zh ? '尚无已知路由授权' : 'No known route grants yet'}</small>}
    </section>
  </>;
}
