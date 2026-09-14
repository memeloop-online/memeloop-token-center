import type { ModelRouteView, ProviderType, UpstreamAccount } from '../types.js';

function shortIdentity(id: string, ids: string[]) {
  for (let length = 4; length < id.length; length++) {
    if (!ids.some(other => other !== id && other.slice(-length) === id.slice(-length))) return `…${id.slice(-length)}`;
  }
  return id;
}

export function credentialRouteOptions(routes: ModelRouteView[], accounts: UpstreamAccount[], providers: ProviderType[], locale: string) {
  const zh = locale.startsWith('zh');
  // Only stable route IDs identify grants; equal model or account names do not.
  const options = [...new Map(routes.map(route => [route.id, route])).values()].map(route => {
    const candidates = route.candidate_upstream_account_ids;
    const ids = [...new Set(candidates ?? route.upstream_account_ids ?? (route.upstream_account_id && route.upstream_account_id !== '00000000-0000-0000-0000-000000000000' ? [route.upstream_account_id] : []))];
    const accountLabels = ids.map(id => {
      const account = accounts.find(value => value.id === id);
      const provider = providers.find(value => value.id === account?.driver);
      const duplicateName = account && accounts.some(value => value.id !== id && value.name === account.name && value.driver === account.driver);
      return `${provider?.display_name ?? account?.driver ?? (zh ? '提供商未知' : 'Unknown provider')} → ${account ? `${account.name}${duplicateName ? ` (${shortIdentity(id, accounts.map(value => value.id))})` : ''}` : `${zh ? '账号' : 'Account'} ${shortIdentity(id, ids)}`}`;
    });
    const state = !route.enabled ? (zh ? '已停用' : 'Disabled')
      : candidates?.length === 0 ? (zh ? '无可用候选账号' : 'No eligible accounts')
      : candidates === undefined ? (zh ? '候选目录未知' : 'Candidate catalog unknown')
      : '';
    return {
      value: route.id,
      label: `${accountLabels.join(' / ') || (zh ? '未找到候选账号' : 'No candidate account found')} → ${route.public_model}`,
      description: [state, `${zh ? '此路由含' : 'Accounts in route:'} ${ids.length} ${zh ? '个账号' : ''}`, route.protocol, route.upstream_model].filter(Boolean).join(' · '),
      details: `${zh ? '路由' : 'Route'}: ${route.id}\n${zh ? '账号' : 'Accounts'}: ${ids.join(', ')}`,
      disabled: !route.enabled || candidates?.length === 0,
    };
  });
  return options.map(option => options.some(other => other.value !== option.value && other.label === option.label && (option.disabled || !other.disabled))
    ? { ...option, label: `${option.label} (${shortIdentity(option.value, options.map(value => value.value))})` }
    : option);
}
