export interface ProxyGroupNavigationContext {
  accountId?: string;
  tenant?: string;
}

export function readProxyGroupNavigationContext(search: string): ProxyGroupNavigationContext {
  const parameters = new URLSearchParams(search);
  if (parameters.get('view') !== 'proxy-groups') return {};
  const accountId = parameters.get('account');
  const tenant = parameters.get('tenant');
  return {
    ...(accountId && accountId.length <= 200 ? { accountId } : {}),
    ...(tenant && tenant.length <= 200 ? { tenant } : {}),
  };
}

export function currentProxyGroupNavigationContext(): ProxyGroupNavigationContext {
  return readProxyGroupNavigationContext(window.location.search);
}

export function withProxyGroupNavigationContext(href: string, context: ProxyGroupNavigationContext): string {
  const url = new URL(href, 'https://navigation.invalid');
  if (url.searchParams.get('view') === 'proxy-groups') {
    if (context.accountId && context.accountId.length <= 200) url.searchParams.set('account', context.accountId);
    if (context.tenant && context.tenant.length <= 200) url.searchParams.set('tenant', context.tenant);
  }
  return `${url.pathname}${url.search}`;
}
