/** Presentation only: keep external IDs unchanged in requests, keys and inputs. */
export function tenantDisplayName(externalId: string, locale: string): string {
  return externalId === 'default' && locale.startsWith('zh') ? '默认' : externalId;
}
