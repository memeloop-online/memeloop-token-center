/** Shared wire/usage/archive oracle for isolated image fixtures. No customer inputs. */
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { setTimeout as delay } from 'node:timers/promises';

type Value = Record<string, any>;
export class HttpFailure extends Error {
  readonly status: number;
  readonly code: string;
  readonly reason: string;
  constructor(status: number, facts = { code: 'unknown', reason: 'unknown' }) { super(`fixture HTTP ${status}`); this.status = status; this.code = facts.code; this.reason = facts.reason; }
}
export function sha256(bytes: string | Uint8Array): string { return createHash('sha256').update(bytes).digest('hex'); }
export function decode(bytes: Uint8Array): string { return new TextDecoder('utf-8', { fatal: true }).decode(bytes); }
export function object(bytes: Uint8Array): Value { const value = JSON.parse(decode(bytes)); assert(value && typeof value === 'object' && !Array.isArray(value)); return value; }
export function segment(value: string): string { return encodeURIComponent(value).replace(/[!'()*]/g, (char) => `%${char.charCodeAt(0).toString(16).toUpperCase()}`); }
export function evidence(value: Value): void { console.log(JSON.stringify(value)); }

// Only small, allowlisted error metadata may leave a synthetic API failure.
export async function errorFacts(response: Response): Promise<{ code: string; reason: string }> {
  const unknown = { code: 'unknown', reason: 'unknown' };
  if (!response.body) return unknown;
  const reader = response.body.getReader(); let length = 0; const chunks: Uint8Array[] = [];
  try {
    for (;;) {
      const frame = await reader.read(); if (frame.done) break;
      length += frame.value.byteLength;
      if (length > 4096) { await reader.cancel(); return unknown; }
      chunks.push(frame.value);
    }
    const value = object(Buffer.concat(chunks)).error;
    const codes = new Set(['archive_content_unavailable']);
    const reasons = new Set(['raw_retired_semantic_archive', 'archive_pending', 'archive_object_unavailable', 'archive_payload_invalid', 'media_body_not_archived_by_policy']);
    return { code: codes.has(value?.code) ? value.code : 'unknown', reason: reasons.has(value?.reason) ? value.reason : 'unknown' };
  } catch { return unknown; }
  finally { reader.releaseLock(); }
}

export function client(base: string, serviceToken: string) {
  const request = async (path: string, method = 'GET', data?: Value, token = serviceToken, timeout = 40_000) => {
    const response = await fetch(base + path, { method, headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), 'content-type': 'application/json', 'accept-encoding': 'identity' }, body: data === undefined ? undefined : JSON.stringify(data), signal: AbortSignal.timeout(timeout) });
    if (!response.ok) throw new HttpFailure(response.status, await errorFacts(response));
    return { status: response.status, headers: response.headers, raw: Buffer.from(await response.arrayBuffer()) };
  };
  return { request, post: async (path: string, data: Value) => object((await request(path, 'POST', data)).raw) };
}

export function verifyStream(body: Buffer): void {
  const wire = decode(body); assert.equal(wire.split('data: [DONE]').length - 1, 1);
  const frames = wire.trim().split('\n\n');
  assert.equal(frames.at(-1), 'data: [DONE]');
  assert.equal(frames.filter((frame) => frame === 'data: [DONE]').length, 1);
  const parsed: Value[] = frames.slice(0, -1).filter((frame) => frame.startsWith('data: ')).map((frame) => JSON.parse(frame.slice(6)));
  assert.equal(parsed.length, frames.length - 1);
  const finish = parsed.flatMap((frame, index) => (frame.choices ?? []).some((choice: Value) => choice.finish_reason === 'stop') ? [index] : []);
  const usage = parsed.flatMap((frame, index) => frame.usage !== undefined && frame.usage !== null ? [index] : []);
  assert.equal(finish.length, 1); assert.equal(usage.length, 1);
  assert(finish[0] < usage[0]); assert.equal(usage[0], parsed.length - 1);
  assert.equal(parsed[usage[0]].usage.prompt_tokens, 2); assert.equal(parsed[usage[0]].usage.completion_tokens, 1);
  assert(parsed.slice(0, finish[0]).some((frame) => (frame.choices ?? []).some((choice: Value) => choice.delta?.content)));
}
export function verifyArchiveCopies(nonce: string, body: Buffer, archives: Record<'request' | 'response', Buffer>): void {
  assert(archives.request.includes(Buffer.from(nonce))); assert.deepEqual(archives.response, body);
}
export const keyPolicy = { requests_per_minute: 60, tokens_per_minute: 100000, max_concurrency: 1, enforcement_mode: 'metered_unlimited', daily_budget: null, weekly_budget: null, lifetime_budget: null };

export async function runImageContract(options: { messages?: (nonce: string) => Value[]; linkedSession?: boolean; captured?: (requestId: string) => void } = {}) {
  const service = process.env.E2E_SERVICE_TOKEN; assert(service, 'E2E_SERVICE_TOKEN required');
  const http = client('http://127.0.0.1:18080', service);
  const nonce = randomUUID().replaceAll('-', ''), tenant = `image-e2e-${nonce}`, model = `${tenant}-model`, session = `${tenant}-session`;
  const upstream = await http.post('/internal/v1/upstreams', { tenant_external_id: tenant, name: model, driver: 'http-json', config: { base_url: 'http://127.0.0.1:18082/scenario/normal', network_scope: 'private' }, credential: { type: 'none' } });
  const route = await http.post('/internal/v1/model-routes', { tenant_external_id: tenant, public_model: model, upstream_account_id: upstream.id, upstream_model: model, protocol: 'openai', priority: 0, custom_model_confirmed: true });
  await http.post(`/internal/v1/prices/USD/${model}`, { input_per_million: '0.01', output_per_million: '0.02' });
  const key = (await http.post('/internal/v1/keys', { tenant_external_id: tenant, principal_external_id: tenant, alias: 'isolated image e2e', currency: 'USD', initial_balance: '100', route_ids: [route.id], policy: keyPolicy })).key as string;
  const fault = client('http://127.0.0.1:19001', '');
  const mode = process.env.E2E_FAULT_MODE ?? 'normal';
  if (mode !== 'normal') await fault.request(`/__test__/${mode}`);
  // This is the real authenticated HTTP request, not a fabricated archive row.
  const response = await fetch('http://127.0.0.1:18080/v1/chat/completions', { method: 'POST', headers: { authorization: `Bearer ${key}`, 'content-type': 'application/json', 'x-mtc-conversation-id': session, 'accept-encoding': 'identity' }, body: JSON.stringify({ model, messages: options.messages?.(nonce) ?? [{ role: 'user', content: `synthetic image e2e ${nonce}` }], stream: true, max_tokens: 1, benchmark: { mode: 'stream', bytes: 4096, chunk_bytes: 4096, delay_ms: 0 } }), signal: AbortSignal.timeout(60_000) });
  const requestId = response.headers.get('x-mtc-request-id'); const body = Buffer.from(await response.arrayBuffer());
  assert.equal(response.status, 200); assert(requestId); verifyStream(body);
  if (mode !== 'normal') {
    const count = Number(decode((await fault.request('/__test__/count')).raw)); assert(Number.isSafeInteger(count) && count > 0, 'fault mode did not encounter an object PUT');
    await fault.request('/__test__/normal'); evidence({ fault_mode: mode, intercepted_puts: count });
  }
  const marker = sha256(`synthetic image e2e ${nonce}`);
  const observed = readFileSync('/tmp/mtc-image-e2e-mock.jsonl', 'utf8').split('\n').filter(Boolean).map((line) => JSON.parse(line));
  assert.equal(observed.filter((row) => row.marker_sha256 === marker).length, 1);
  options.captured?.(requestId);
  evidence({ request_id: requestId, stream_status: 200, stream_bytes: body.length, stream_sha256: sha256(body), terminal: true });
  let record: Value | undefined;
  for (let attempt = 0; attempt < 90; attempt++) {
    try {
      const candidate = object((await http.request(`/self/v1/requests/${requestId}`, 'GET', undefined, key)).raw);
      if (candidate.archive_complete && candidate.completed_at && candidate.usage?.tokens) { record = candidate; break; }
    } catch (error) { if (!(error instanceof HttpFailure)) throw error; }
    await delay(2000);
  }
  assert(record, 'request terminal, usage and archive did not converge'); assert.equal(record.request_id, requestId);
  assert.equal(record.usage.tokens.input_tokens, 2); assert.equal(record.usage.tokens.output_tokens, 1);
  const archives = {} as Record<'request' | 'response', Buffer>;
  for (const side of ['request', 'response'] as const) {
    let result;
    try { result = await http.request(`/self/v1/requests/${requestId}/archive/${side}`, 'GET', undefined, key); }
    catch (error) {
      if (error instanceof HttpFailure) evidence({ phase: 'archive_get_failed', request_id: requestId, side, status: error.status, error_code: error.code, reason: error.reason, observed_at: new Date().toISOString() });
      throw error;
    }
    assert.equal(result.status, 200); assert(result.raw.length); archives[side] = result.raw;
    evidence({ request_id: requestId, side, status: result.status, bytes: result.raw.length, sha256: sha256(result.raw) });
  }
  verifyArchiveCopies(nonce, body, archives);
  let matched: string[] = [], listing: Value = {}, sessionStatus = 0;
  for (let attempt = 0; attempt < 90; attempt++) {
    listing = object((await http.request('/self/v1/sessions?limit=100', 'GET', undefined, key)).raw); matched = [];
    for (const entry of listing.sessions) {
      if (options.linkedSession && (entry.unlinked || !entry.cluster_id)) continue;
      try {
        const result = await http.request(`/self/v1/sessions/${segment(entry.session_id)}`, 'GET', undefined, key); const item = object(result.raw); sessionStatus = result.status;
        if (result.status === 200 && item.requests.some((row: Value) => row.request_id === requestId)) matched.push(entry.session_id);
      } catch (error) { if (!(error instanceof HttpFailure)) throw error; }
    }
    if (matched.length === 1) break;
    await delay(2000);
  }
  if (matched.length !== 1) { evidence({ session_list_count: listing.sessions?.length, matching_request_count: matched.length, request_id: requestId, worker_role: 'all' }); throw new Error('same request did not appear in session projection'); }
  evidence({ request_id: requestId, session_status: sessionStatus, usage: record.usage, archive_complete: true });
  return { http, nonce, key, requestId, sessionId: matched[0] };
}
