import type { ProviderType } from '../types';

/** Provider metadata names a service; an upstream account's name names an identity. */
export function providerDisplayName(driver: string | undefined, providers: ProviderType[], locale: string) {
  return providers.find(provider => provider.id === driver)?.display_name
    || (locale.startsWith('zh') ? '未知提供商' : 'Unknown provider');
}
