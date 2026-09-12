/** Defense in depth only: the authenticated server must authorize and validate first. */
export type PluginUiComponent =
  | { kind: 'text'; text: string }
  | { kind: 'metric'; label: string; value: string }
  | { kind: 'status'; label: string; state: 'ok' | 'warning' | 'error' | 'unknown' }
  | { kind: 'link'; label: string; href: string };

export interface PluginUiProjection {
  schema_version: 1;
  plugin_id: string;
  slot_id: string;
  components: PluginUiComponent[];
}

export interface PluginUiPolicy {
  pluginId: string;
  slotId: string;
  /** Core-owned exact HTTPS origins, never taken from the projection itself. */
  allowedLinkOrigins: readonly string[];
}

function record(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === 'object' && !Array.isArray(value);
}
function keys(value: Record<string, unknown>, allowed: string[]): boolean {
  return Object.keys(value).length === allowed.length && allowed.every((key) => Object.hasOwn(value, key));
}
function boundedText(value: unknown, max: number): value is string {
  return typeof value === 'string' && value.length > 0 && value.length <= max && !/[\u0000-\u001f\u007f\u202a-\u202e\u2066-\u2069]/u.test(value);
}
function safeLink(value: unknown, origins: readonly string[]): value is string {
  if (!boundedText(value, 2048) || !value.startsWith('https://') || /[\s\\]/u.test(value)) return false;
  try {
    const url = new URL(value);
    return url.protocol === 'https:' && !url.username && !url.password && origins.includes(url.origin);
  } catch { return false; }
}

/** Reject whole invalid slots; no truncation that might hide an error or change a link. */
export function parsePluginUiProjection(value: unknown, policy: PluginUiPolicy): PluginUiProjection | null {
  if (!record(value) || !keys(value, ['schema_version', 'plugin_id', 'slot_id', 'components'])) return null;
  if (value.schema_version !== 1 || value.plugin_id !== policy.pluginId || value.slot_id !== policy.slotId) return null;
  if (!boundedText(value.plugin_id, 128) || !boundedText(value.slot_id, 128)) return null;
  if (!Array.isArray(value.components) || value.components.length > 32) return null;
  const components: PluginUiComponent[] = [];
  for (const component of value.components) {
    if (!record(component)) return null;
    if (component.kind === 'text' && keys(component, ['kind', 'text']) && boundedText(component.text, 2048)) {
      components.push({ kind: 'text', text: component.text });
    } else if (component.kind === 'metric' && keys(component, ['kind', 'label', 'value']) && boundedText(component.label, 128) && boundedText(component.value, 128)) {
      components.push({ kind: 'metric', label: component.label, value: component.value });
    } else if (component.kind === 'status' && keys(component, ['kind', 'label', 'state']) && boundedText(component.label, 128) && ['ok', 'warning', 'error', 'unknown'].includes(String(component.state))) {
      components.push({ kind: 'status', label: component.label, state: component.state as 'ok' | 'warning' | 'error' | 'unknown' });
    } else if (component.kind === 'link' && keys(component, ['kind', 'label', 'href']) && boundedText(component.label, 128) && safeLink(component.href, policy.allowedLinkOrigins)) {
      components.push({ kind: 'link', label: component.label, href: component.href });
    } else return null;
  }
  return { schema_version: 1, plugin_id: value.plugin_id, slot_id: value.slot_id, components };
}
