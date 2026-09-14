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
    const candidateAccounts = ids.map(id => {
      const account = accounts.find(value => value.id === id);
      const provider = providers.find(value => value.id === account?.driver);
      const duplicateName = account && accounts.some(value => value.id !== id && value.name === account.name && value.driver === account.driver);
      return {
        id,
        provider: provider?.display_name ?? account?.driver ?? (zh ? '提供商未知' : 'Unknown provider'),
        name: account ? `${account.name}${duplicateName ? ` (${shortIdentity(id, accounts.map(value => value.id))})` : ''}` : `${zh ? '账号' : 'Account'} ${shortIdentity(id, ids)}`,
      };
    });
    const state = !route.enabled ? (zh ? '已停用' : 'Disabled')
      : candidates?.length === 0 ? (zh ? '无可用候选账号' : 'No eligible accounts')
      : candidates === undefined ? (zh ? '候选目录未知' : 'Candidate catalog unknown')
      : '';
    const scope = candidates === undefined ? (zh ? '候选范围未确认' : 'Candidate scope unconfirmed')
      : ids.length > 1 ? (zh ? `共享候选 · ${ids.length} 个账号` : `Shared candidates · ${ids.length} accounts`)
      : ids.length === 1 ? (zh ? '单账号' : 'Single account')
      : (zh ? '无候选账号' : 'No candidate accounts');
    const providerNames = [...new Set(candidateAccounts.map(account => account.provider))].join(' / ');
    const accountDescription = candidateAccounts.map(account => `${account.name} · ${account.provider}`).join(' / ');
    return {
      value: route.id,
      label: route.public_model,
      chipDescription: [scope, ids.length === 1 ? candidateAccounts[0].name : providerNames].filter(Boolean).join(' · '),
      description: [state, scope, accountDescription, route.protocol, route.upstream_model !== route.public_model ? `${zh ? '上游' : 'Upstream'}: ${route.upstream_model}` : ''].filter(Boolean).join(' · '),
      accountDescription,
      scope,
      details: [
        `${zh ? '模型' : 'Model'}: ${route.public_model}`,
        `${zh ? '账号范围' : 'Account scope'}: ${scope}`,
        ...candidateAccounts.map(account => `${zh ? '提供商' : 'Provider'}: ${account.provider}; ${zh ? '账号' : 'Account'}: ${account.name}; ID: ${account.id}`),
        `${zh ? '上游模型' : 'Upstream model'}: ${route.upstream_model}`,
        `${zh ? '协议' : 'Protocol'}: ${route.protocol}`,
        `${zh ? '路由 ID' : 'Route ID'}: ${route.id}`,
        state,
      ].filter(Boolean).join('\n'),
      disabled: !route.enabled || candidates?.length === 0,
    };
  });
  return options.map(option => options.some(other => other.value !== option.value && other.label === option.label)
    ? { ...option, label: `${option.label} (${shortIdentity(option.value, options.map(value => value.value))})` }
    : option);
}
