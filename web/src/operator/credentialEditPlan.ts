import type { CredentialRoutingView, KeyView } from '../types.js';

export type CredentialEditSection = 'alias' | 'policy' | 'routing';

export interface CredentialEditOperation {
  section: CredentialEditSection;
  method: 'PATCH' | 'PUT';
  suffix: 'alias' | 'policy' | 'routing';
  body: unknown;
}

function stableStringify(value: unknown): string {
  if (value === null || typeof value !== 'object') return JSON.stringify(value) ?? '';
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(',')}]`;
  const entries = Object.entries(value as Record<string, unknown>)
    .filter(([, entry]) => entry !== undefined)
    .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
    .map(([key, entry]) => `${JSON.stringify(key)}:${stableStringify(entry)}`);
  return `{${entries.join(',')}}`;
}

function sameIds(current: string[], draft: string[]): boolean {
  return stableStringify([...current].sort()) === stableStringify([...draft].sort());
}

/**
 * One edit panel covers every field the create form can write. Only changed
 * sections produce a save operation, so saving an untouched policy or routing
 * section never rewrites what the operator did not intend to change.
 */
export function credentialEditOperations(
  current: { alias: string; policy: KeyView['policy']; routing?: CredentialRoutingView },
  draft: { alias: string; policy: unknown; routing?: Pick<CredentialRoutingView, 'route_ids' | 'route_group_ids' | 'grant_revision'> },
  tenant: string,
): CredentialEditOperation[] {
  const operations: CredentialEditOperation[] = [];
  if (draft.alias !== current.alias) {
    operations.push({ section: 'alias', method: 'PATCH', suffix: 'alias', body: { alias: draft.alias } });
  }
  if (stableStringify(draft.policy) !== stableStringify(current.policy)) {
    operations.push({ section: 'policy', method: 'PUT', suffix: 'policy', body: draft.policy });
  }
  if (draft.routing && current.routing
    && (!sameIds(current.routing.route_ids, draft.routing.route_ids) || !sameIds(current.routing.route_group_ids, draft.routing.route_group_ids))) {
    operations.push({
      section: 'routing', method: 'PUT', suffix: 'routing',
      body: {
        tenant_external_id: tenant,
        route_ids: draft.routing.route_ids,
        route_group_ids: draft.routing.route_group_ids,
        expected_grant_revision: draft.routing.grant_revision,
      },
    });
  }
  return operations;
}

export function credentialAliasReady(alias: string): boolean {
  return alias.trim().length > 0;
}
