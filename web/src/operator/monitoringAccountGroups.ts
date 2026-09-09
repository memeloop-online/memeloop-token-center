import type { OperatorMonitoringSnapshot } from '../types.js';

type UpstreamModel = OperatorMonitoringSnapshot['top_upstream_models'][number];

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
