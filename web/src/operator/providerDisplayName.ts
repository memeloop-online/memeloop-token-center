import type { ProviderType } from '../types';

/** Provider metadata names a service; an upstream account's name names an identity. */
export function canonicalProviderDriver(driver: string | undefined): string | undefined {
  if (driver === undefined) return undefined;
  if (driver === 'cbcnx' || driver === 'openai-compatible') return 'http-json';
  if (driver === 'one-api') return 'new-api';
  return driver;
}

export function providerDisplayName(driver: string | undefined, providers: ProviderType[], locale: string) {
  return providers.find(provider => provider.id === canonicalProviderDriver(driver))?.display_name
    || (locale.startsWith('zh') ? '未知提供商' : 'Unknown provider');
}
