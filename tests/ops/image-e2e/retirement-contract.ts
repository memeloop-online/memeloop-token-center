/** Isolated fixture only: exact metadata reads, never a replacement raw-body reader. */
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';

type Value = Record<string, any>;
const utf8 = (bytes: Uint8Array) => new TextDecoder('utf-8', { fatal: true }).decode(bytes);
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const digest = /^[0-9a-f]{64}$/;
export function preciseJson(text: string): Value {
  return JSON.parse(text, (_key, value, context) => typeof value === 'number' && !Number.isSafeInteger(value) ? context.source : value);
}
export function blake3(bytes: Uint8Array): string {
  const result = spawnSync('b3sum', ['--no-names'], { input: bytes, encoding: 'utf8', timeout: 15_000, maxBuffer: 1024 });
  assert.equal(result.status, 0, 'trusted BLAKE3 CLI unavailable'); const value = result.stdout.trim(); assert(digest.test(value)); return value;
}
function generation(revision: string): string {
  const hex = BigInt(revision).toString(16).padStart(32, '0'); assert.equal(hex.length, 32);
  return `${hex.slice(0,8)}-${hex.slice(8,12)}-${hex.slice(12,16)}-${hex.slice(16,20)}-${hex.slice(20)}`;
}
function complete(value: Value): void {
  assert.equal(value.observed_terminal, true); assert.equal(value.projection_state, 'complete'); assert.deepEqual(value.gaps, []);
}
export function verifyRetirementProof(scope: Value, side: 'request' | 'response', fence: Value, receipt: Value, publication: Value, manifest: Value): void {
  assert(uuid.test(scope.tenant_id) && uuid.test(scope.request_id)); assert(digest.test(scope.leaf));
  for (const value of [fence, receipt.publication, receipt.binding, publication, manifest]) {
    assert.equal(value.tenant_id, scope.tenant_id); assert.equal(value.request_id, scope.request_id);
  }
  assert.equal(fence.format_version, 1); assert.equal(fence.side, side);
  assert.equal(fence.release_generation, generation(scope.revision));
  assert.equal(String(fence.semantic_revision), scope.revision); assert.equal(fence.semantic_leaf, scope.leaf);
  assert.equal(receipt.format_version, 2); assert.equal(publication.format_version, 2); assert.equal(manifest.format_version, 2);
  assert.deepEqual(receipt.publication, publication);
  assert.equal(String(publication.revision), scope.revision); assert.equal(String(manifest.revision), scope.revision);
  assert.equal(manifest.projection_version, publication.projection_version);
  assert.equal(receipt.binding.node_hash, scope.leaf); assert.equal(manifest.semantic_head.node_hash, scope.leaf);
  assert(Number(receipt.binding.projected_at) > 0); complete(receipt.coverage); complete(manifest.coverage);
  assert.deepEqual(receipt.coverage, manifest.coverage); assert.deepEqual(receipt.raw_sources, manifest.raw_sources);
  for (const name of ['request', 'response']) {
    const source = manifest.raw_sources[name]; assert(source && digest.test(source.blake3_digest));
    assert(Number.isSafeInteger(source.size_bytes) && source.size_bytes > 0);
    // This fixture has one primary store. Qualified alternative locators need their own authorized fixture.
    assert.equal(source.object_locator, `tenants/${scope.tenant_id}/cas/v1/blake3/${source.blake3_digest.slice(0, 2)}/${source.blake3_digest}` + (source.object_locator.endsWith('.mtcz1') ? '.mtcz1' : ''));
    assert.equal(source.size_bytes, scope.lengths[name]); assert.equal(source.blake3_digest, scope.digests[name]);
  }
  assert.deepEqual(fence.source, manifest.raw_sources[side]);
}
function command(binary: string, args: string[], limit: number): string {
  const result = spawnSync(binary, args, { encoding: 'utf8', timeout: 15_000, maxBuffer: limit });
  assert.equal(result.status, 0, 'isolated proof command failed (output withheld)'); return result.stdout;
}
export function fixtureIdentity(requestId: string, keyId: string): Value {
  assert.equal(process.env.GITHUB_ACTIONS, 'true', 'metadata adapter is GHA-isolated only');
  assert(uuid.test(requestId) && uuid.test(keyId));
  // UUIDs validated before literal interpolation; read-only and exact identity, no payload columns.
  const sql = `BEGIN READ ONLY; SET LOCAL statement_timeout='3s'; SELECT json_build_object('tenant_id',r.tenant_id,'request_id',r.id,'key_id',r.key_id,'ledger_count',(SELECT count(*) FROM ledger_entries l WHERE l.source=r.reservation_id AND l.kind='usage'),'unsettled',(SELECT count(*) FROM conversation_projection_outbox o WHERE o.request_id=r.id AND o.tenant_id=r.tenant_id AND o.key_id=r.key_id AND (o.projected_at IS NULL OR (o.lease_owner IS NOT NULL AND o.lease_owner <> 'terminal-v118') OR o.terminal_lease_owner IS NOT NULL))) FROM request_records r WHERE r.id='${requestId}' AND r.key_id='${keyId}'; ROLLBACK;`;
  const lines = command('docker', ['exec', 'image-e2e-postgres', 'psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-U', 'token_center', '-d', 'token_center_test', '-c', sql], 16_384).trim().split('\n');
  assert.equal(lines.length, 1); const row = preciseJson(lines[0]); assert.equal(row.ledger_count, 1); assert.equal(row.unsettled, 0); return row;
}
function exactMetadataBytes(key: string): Buffer {
  assert.equal(process.env.GITHUB_ACTIONS, 'true', 'metadata adapter is GHA-isolated only');
  assert(/^tenants\/[0-9a-f-]+\/(semantic\/v2\/requests\/[0-9a-f-]+\/(?:[0-9]+(?:-coverage)?|retired-(?:request|response))\.json|cas\/v1\/blake3\/[a-f0-9/]+)$/.test(key), 'unsupported metadata locator');
  const password = process.env.E2E_S3_PASSWORD; assert(password, 'isolated S3 fixture credential required');
  // Bypass only the fault proxy, never the configured store; no LIST/HEAD/body source reads.
  return Buffer.from(command('curl', ['--silent', '--fail', '--max-time', '10', '--max-filesize', '262144', '--aws-sigv4', 'aws:amz:us-east-1:s3', '--user', `rustfs-test-fixture:${password}`, `http://127.0.0.1:19000/image-e2e/${key}`], 262_144), 'utf8');
}
export function exactFixtureMetadata(key: string): Value { return preciseJson(utf8(exactMetadataBytes(key))); }
export function boundManifest(publication: Value, scope: Value): Value {
  assert.equal(publication.format_version, 2); assert.equal(publication.tenant_id, scope.tenant_id);
  assert.equal(publication.request_id, scope.request_id); assert.equal(String(publication.revision), scope.revision);
  const reference = publication.manifest; assert(reference && digest.test(reference.blake3_digest));
  assert(Number.isSafeInteger(reference.size_bytes) && reference.size_bytes > 0 && reference.size_bytes <= 262144);
  assert.equal(reference.object_locator, `tenants/${scope.tenant_id}/cas/v1/blake3/${reference.blake3_digest.slice(0,2)}/${reference.blake3_digest}`);
  const bytes = exactMetadataBytes(reference.object_locator); assert.equal(bytes.length, reference.size_bytes);
  assert.equal(blake3(bytes), reference.blake3_digest); return preciseJson(utf8(bytes));
}

export async function verifyPublishedContents(http: any, key: string, sessionId: string, requestId: string, revision: string, expected: { role: string; text: string }[]): Promise<void> {
  const path = `/self/v1/sessions/${encodeURIComponent(sessionId)}/requests/${requestId}/semantic-view`;
  const cursors = new Set<string>(); let reads = 0;
  const get = async (query = '') => { assert(++reads <= 1024, 'synthetic metadata read bound'); if (query) { assert(!cursors.has(query), 'repeated semantic cursor'); cursors.add(query); } return preciseJson(utf8((await http.request(path + query, 'GET', undefined, key)).raw)); };
  let page = await get(); const instances: Value[] = []; const seen = new Set<string>();
  for (;;) {
    assert.equal(page.status, 'published'); assert.equal(page.manifest_revision, revision); complete(page.coverage);
    for (const instance of page.instances) { assert(!seen.has(instance.instance_id)); seen.add(instance.instance_id); instances.push(instance); assert(instances.length <= 128); }
    if (!page.next_cursor) break;
    page = await get('?' + new URLSearchParams({ cursor: page.next_cursor }));
  }
  const actual: { role: string; text: string }[] = [];
  for (const instance of instances) {
    assert.equal(instance.request_id, requestId); let parts = await get('?' + new URLSearchParams({ parts_cursor: instance.atom_cursor })); let text = '';
    for (;;) {
      assert.equal(parts.manifest_revision, revision); assert.equal(parts.instance_id, instance.instance_id);
      for (const part of parts.parts) {
        assert(!part.media, 'retired image fixture requires explicit media oracle');
        for (const field of part.fields ?? []) {
          assert.equal(field.format, 'text'); assert(field.content_cursor); let cursor = field.content_cursor; let offset = 0;
          for (;;) {
            const query = '?' + new URLSearchParams({ content_cursor: cursor }); assert(!cursors.has(query)); cursors.add(query); assert(++reads <= 1024);
            const response = await http.request(path + query, 'GET', undefined, key);
            const frames = utf8(response.raw).split(/\r?\n/).filter(Boolean).map((line: string) => preciseJson(line)); assert(frames.length);
            for (const frame of frames) {
              assert.equal(frame.manifest_revision, revision); assert.equal(frame.instance_id, instance.instance_id); assert.equal(frame.byte_offset, offset);
              assert.equal(frame.field, field.name); assert.equal(typeof frame.text, 'string'); text += frame.text; offset += Buffer.byteLength(frame.text);
            }
            const last = frames.at(-1)!; if (last.complete === true) { assert(!last.next_cursor); break; }
            assert(last.next_cursor); cursor = last.next_cursor;
          }
        }
      }
      if (!parts.next_cursor) break;
      parts = await get('?' + new URLSearchParams({ parts_cursor: parts.next_cursor }));
    }
    actual.push({ role: parts.role, text });
  }
  assert.deepEqual(actual, expected, 'formal semantic API content differs from synthetic request/response');
}
