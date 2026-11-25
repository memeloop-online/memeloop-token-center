import type { SelectedRequestEgress } from '../types.js';
import { requestEgressCopy } from './requestEgressCopy.js';

export function egressAddress(value: { scheme?: string | null; host?: string | null; port?: number | null }, locale: string): string {
  const { scheme, host, port } = value;
  if (!scheme || !['socks5h', 'socks5', 'http', 'https'].includes(scheme) || !host
    || /[\s/@?#\\]/.test(host) || !Number.isInteger(port) || port! < 1 || port! > 65535) return requestEgressCopy(locale).unknown;
  const hostname = host.includes(':') && !host.startsWith('[') ? `[${host}]` : host;
  return `${scheme}://${hostname}:${port}`;
}

export function egressBytes(value: number | null | undefined, locale: string): string {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) return requestEgressCopy(locale).unknown;
  const units = ['B', 'KB', 'MB', 'GB'];
  const unit = value === 0 ? 0 : Math.max(0, Math.min(units.length - 1, Math.floor(Math.log(value) / Math.log(1000))));
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: unit === 0 ? 0 : 2 }).format(value / 1000 ** unit)} ${units[unit]}`;
}

export function egressSelectionName(selected: SelectedRequestEgress, locale: string): string {
  const copy = requestEgressCopy(locale);
  if (selected.kind === 'direct') return copy.direct;
  if (selected.kind !== 'proxy') return copy.unknown;
  return [selected.group_name || (selected.group_id ? copy.unnamedGroup : copy.proxy), selected.member_name || copy.unnamedMember].join(' · ');
}

export function egressSelectionKey(selected: SelectedRequestEgress): string {
  return JSON.stringify([selected.kind, selected.group_id, selected.group_version, selected.group_name,
    selected.member_id, selected.member_name, selected.scheme, selected.host, selected.port]);
}

export function egressGroupKey(selected: SelectedRequestEgress): string {
  return selected.group_id ?? (selected.kind === 'proxy' ? 'ungrouped' : selected.kind);
}

export function egressSummaryPath(tenant: string, start: number, end: number): string {
  const query = new URLSearchParams({ start: String(start), end: String(end) });
  if (tenant) query.set('tenant_external_id', tenant);
  return `/internal/v1/requests/egress-summary?${query}`;
}
