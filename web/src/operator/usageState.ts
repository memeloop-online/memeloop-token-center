export type UsageTab = 'overview' | 'trend' | 'dimensions' | 'heatmap';
export type Preset = '24h' | 'today' | 'yesterday' | '7d' | '30d' | 'custom';
export type Granularity = 'auto' | 'hour' | 'day';
export interface UsageFilters { model: string; keyId: string; upstreamId: string; protocol: string; status: string; errorCode: string }
export interface UsageSelection { preset: Preset; granularity: Granularity; customFrom: string; customTo: string; filters: UsageFilters }

export const usageTabs: UsageTab[] = ['overview', 'trend', 'dimensions', 'heatmap'];

export function nextUsageTab(current: UsageTab, key: string) {
  const index = usageTabs.indexOf(current);
  if (key === 'Home') return usageTabs[0];
  if (key === 'End') return usageTabs[usageTabs.length - 1];
  if (key === 'ArrowRight') return usageTabs[(index + 1) % usageTabs.length];
  if (key === 'ArrowLeft') return usageTabs[(index - 1 + usageTabs.length) % usageTabs.length];
  return undefined;
}

export function localDateTimeInput(epoch: number) {
  const date = new Date(epoch);
  return new Date(epoch - date.getTimezoneOffset() * 60_000).toISOString().slice(0, 23);
}

function rangeFor(selection: UsageSelection, now = Date.now()) {
  const end = now;
  if (selection.preset === '24h') return { from: end - 86_400_000, to: end };
  if (selection.preset === '7d') return { from: end - 7 * 86_400_000, to: end };
  if (selection.preset === '30d') return { from: end - 30 * 86_400_000, to: end };
  const today = new Date(now); today.setHours(0, 0, 0, 0);
  if (selection.preset === 'today') return { from: today.getTime(), to: end };
  if (selection.preset === 'yesterday') { const yesterday = new Date(today); yesterday.setDate(yesterday.getDate() - 1); return { from: yesterday.getTime(), to: today.getTime() - 1 }; }
  const from = Date.parse(selection.customFrom); const to = Date.parse(selection.customTo);
  if (!Number.isFinite(from) || !Number.isFinite(to) || from > to) return undefined;
  return { from, to };
}

export function statsQuery(tenant: string, selection: UsageSelection) {
  const range = rangeFor(selection); if (!range) return undefined;
  const params = new URLSearchParams({ from_created_at: String(range.from), to_created_at: String(range.to), granularity: selection.granularity });
  if (tenant) params.set('tenant_external_id', tenant);
  if (selection.filters.model.trim()) params.set('model', selection.filters.model.trim());
  if (selection.filters.keyId.trim()) params.set('key_id', selection.filters.keyId.trim());
  if (selection.filters.upstreamId) params.set('upstream_account_id', selection.filters.upstreamId);
  if (selection.filters.protocol) params.set('protocol', selection.filters.protocol);
  if (selection.filters.status) params.set('status', selection.filters.status);
  if (selection.filters.errorCode.trim()) params.set('error_code', selection.filters.errorCode.trim());
  return `?${params}`;
}
