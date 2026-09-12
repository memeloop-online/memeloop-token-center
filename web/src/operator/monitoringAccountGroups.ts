import type { OperatorMonitoringSnapshot } from '../types.js';

type UpstreamModel = OperatorMonitoringSnapshot['top_upstream_models'][number];

/** One heading per exact model, with unmodified account facts underneath.
 * Latency percentiles and currencies cannot be combined in presentation code.
 * Preserve server rank and distinct identities even when account names match.
 */
export function monitoringModelGroups(rows: readonly UpstreamModel[]) {
  const groups = new Map<string, { model: string; accounts: UpstreamModel[] }>();
  const identities = new Map<string, Set<string>>();
  for (const row of rows) {
    let group = groups.get(row.model);
    if (!group) {
      group = { model: row.model, accounts: [] };
      groups.set(row.model, group);
      identities.set(row.model, new Set());
    }
    const seen = identities.get(row.model)!;
    if (seen.has(row.upstream_account_id)) continue;
    seen.add(row.upstream_account_id);
    group.accounts.push(row);
  }
  return [...groups.values()];
}

/** Presentation grouping only: retain each ranked account/model fact intact. */
export function monitoringAccountGroups(rows: readonly UpstreamModel[]) {
  const groups = new Map<string, { id: string; name: string; models: UpstreamModel[] }>();
  for (const row of rows) {
    let group = groups.get(row.upstream_account_id);
    if (!group) {
      group = { id: row.upstream_account_id, name: row.upstream_name, models: [] };
      groups.set(row.upstream_account_id, group);
    }
    group.models.push(row);
  }
  return [...groups.values()];
}
