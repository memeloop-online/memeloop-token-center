import assert from 'node:assert/strict';
import { writeFileSync } from 'node:fs';
import { setTimeout as delay } from 'node:timers/promises';
import { decode, evidence, HttpFailure, keyPolicy, object, runImageContract, segment } from '../image-e2e/image-contract.ts';

const fixture = await runImageContract({
  linkedSession: true,
  captured: (requestId) => writeFileSync('/tmp/session-e2e-request-id', requestId),
  messages: (nonce) => {
    const messages: Record<string, any>[] = [{ role: 'user', content: `synthetic image e2e ${nonce}` }, ...Array.from({ length: 66 }, (_, index) => ({ role: 'user', content: `page-marker-${nonce}-${index + 1}` }))];
    messages[1].content = `large-body-${nonce}${'😀'.repeat(40000)}`;
    messages[33] = { role: 'user', content: [{ type: 'image_url', image_url: { url: 'data:image/png;base64,c3ludGhldGljLW1lZGlh' } }] };
    return messages;
  },
});
const { http, nonce, key, requestId, sessionId } = fixture;
const path = `/self/v1/sessions/${segment(sessionId)}/requests/${requestId}/semantic-view`;
let page: Record<string, any> | undefined, lastStatus: number | undefined, lastState: unknown;
for (let attempt = 0; attempt < 120; attempt++) {
  try {
    const response = await http.request(path, 'GET', undefined, key); const candidate = object(response.raw); lastStatus = response.status; lastState = candidate.status;
    if (candidate.status === 'published') { page = candidate; break; }
  } catch (error) { if (error instanceof HttpFailure) lastStatus = error.status; else throw error; }
  await delay(2000);
}
if (!page) { evidence({ semantic_http: lastStatus, semantic_state: lastState, session_shape: sessionId.slice(0, 9), request_id: requestId }); throw new Error('semantic manifest did not publish'); }
const instances: Record<string, any>[] = []; let pages = 0;
while (true) {
  pages++; instances.push(...page.instances); const cursor = page.next_cursor;
  if (!cursor) break;
  page = object((await http.request(`${path}?${new URLSearchParams({ cursor })}`, 'GET', undefined, key)).raw);
}
evidence({ semantic_pages: pages, semantic_instances: instances.length, request_id: requestId });
assert(pages >= 2 && instances.length >= 67); assert.equal(new Set(instances.map((item) => item.instance_id)).size, instances.length);
let foundMedia = false, foundLarge = false;
for (const item of instances) {
  const parts = object((await http.request(`${path}?${new URLSearchParams({ parts_cursor: item.atom_cursor })}`, 'GET', undefined, key)).raw);
  for (const part of parts.parts ?? []) {
    const media = part.media?.read_cursor;
    if (media) { const response = await http.request(`${path}?${new URLSearchParams({ media_cursor: media })}`, 'GET', undefined, key); assert.equal(response.status, 200); assert.deepEqual(response.raw, Buffer.from('synthetic-media')); foundMedia = true; }
    for (const field of part.fields ?? []) {
      const cursor = field.content_cursor; if (!cursor) continue;
      const response = await http.request(`${path}?${new URLSearchParams({ content_cursor: cursor })}`, 'GET', undefined, key);
      const frames = decode(response.raw).split(/\r?\n/).filter(Boolean).map((line) => JSON.parse(line));
      const text = frames.map((frame) => frame.text ?? '').join('');
      if (text === `large-body-${nonce}${'😀'.repeat(40000)}`) { assert(frames.length && frames.at(-1).complete === true); foundLarge = true; }
    }
  }
}
evidence({ media_found: foundMedia, large_text_found: foundLarge }); assert(foundMedia && foundLarge);
const other = (await http.post('/internal/v1/keys', { tenant_external_id: `other-${nonce}`, principal_external_id: `other-${nonce}`, alias: 'other tenant', currency: 'USD', initial_balance: '100', route_ids: [], policy: keyPolicy })).key;
let denied = false;
try { await http.request(path, 'GET', undefined, other); }
catch (error) { if (!(error instanceof HttpFailure)) throw error; assert([403, 404].includes(error.status)); denied = true; }
assert(denied, 'cross-tenant semantic read succeeded');
evidence({ request_id: requestId, semantic_pages: pages, instances: instances.length, large_text_verified: foundLarge, media_bytes: 15, cross_tenant_denied: true });
