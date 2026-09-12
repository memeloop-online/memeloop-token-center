import type { OperatorMonitoringSnapshot } from '../types.js';

type UpstreamModel = OperatorMonitoringSnapshot['top_upstream_models'][number];

/** One heading per exact model, with unmodified account facts underneath.
 * Latency percentiles and currencies cannot be combined in presentation code.
 * Grouping is not a ranking. Preserve every fact, including duplicate pairs;
 * the client cannot decide whether overlapping facts may be discarded.
 */
export function monitoringModelGroups(rows: readonly UpstreamModel[]) {
  const groups = new Map<string, { model: string; accounts: UpstreamModel[] }>();
  for (const row of rows) {
    let group = groups.get(row.model);
    if (!group) {
      group = { model: row.model, accounts: [] };
      groups.set(row.model, group);
    }
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
