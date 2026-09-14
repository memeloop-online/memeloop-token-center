import type { RJSFSchema } from '@rjsf/utils';

/** Mirrors the Codex backend private-IP policy; generic proxy schemas stay separate. */
export function isPrivateProxyUrl(value: string) {
  try {
    const url = new URL(value);
    if (url.protocol !== 'socks5h:' || (url.port && Number(url.port) === 0) || url.search || url.hash || (url.pathname && url.pathname !== '/')) return false;
    const host = url.hostname;
    if (host.startsWith('[')) return /^\[f[cd][0-9a-f]{2}:/i.test(host);
    const octets = host.split('.').map(Number);
    if (octets.length !== 4 || octets.join('.') !== host || !octets.every((part) => Number.isInteger(part) && part >= 0 && part <= 255)) return false;
    if (host === '100.100.100.200') return false;
    return octets[0] === 10 || (octets[0] === 172 && octets[1] >= 16 && octets[1] <= 31)
      || (octets[0] === 192 && octets[1] === 168) || (octets[0] === 100 && octets[1] >= 64 && octets[1] <= 127);
  } catch { return false; }
}

export function connectionSchema(schema: RJSFSchema, endpointHint: string): RJSFSchema {
  const result = structuredClone(schema);
  const base = result.properties?.base_url;
  if (base && typeof base === 'object') {
    base.title = 'Base URL';
    base.description = endpointHint;
    if (base.const !== undefined) base.readOnly = true;
  }
  const policy = result.properties?.transport_policy;
  if (policy && typeof policy === 'object') {
    policy.title = 'Runtime retry policy';
    for (const [name, title] of Object.entries({ connect_timeout_millis: 'Connect timeout (ms)', read_timeout_millis: 'Read inactivity timeout (ms)', request_timeout_millis: 'Total request timeout (ms)' })) {
      const field = policy.properties?.[name];
      if (field && typeof field === 'object') field.title = title;
    }
    for (const [name, title] of Object.entries({ version: 'Policy version', candidate_attempts: 'Candidate attempts', failover_deadline_millis: 'Failover deadline (ms)', connect_attempts: 'Connect attempts', connect_retry_delay_millis: 'Connect retry delay (ms)', shared_probe_attempts: 'Shared probe attempts' })) {
      const field = policy.properties?.[name];
      if (field && typeof field === 'object') field.title = title;
    }
  }
  return result;
}
