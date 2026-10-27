import assert from 'node:assert/strict';
import test from 'node:test';
import { isUnlinkedSession, unlinkedRequestFilter } from '../src/operator/sessionNavigation.js';
import type { TypedFilterAst } from '../src/types.js';

const keyId = '5dae9b5a-6bd0-54a7-ae14-fb5aa4f61650';
const bucketId = `unlinked:${keyId}`;

test('credential buckets open request filters without inventing a session association', () => {
  const source: TypedFilterAst = {
    logical_operator: 'and',
    conditions: [
      { field: 'created_at', operator: 'between', value: { type: 'timestamp', value: 1000 }, upper: { type: 'timestamp', value: 2000 } },
      { field: 'protocol', operator: 'equals', value: { type: 'protocol', value: 'openai' } },
      { field: 'model', operator: 'equals', value: { type: 'model', value: 'codex-model' } },
      { field: 'key_id', operator: 'equals', value: { type: 'uuid', value: keyId } },
    ],
  };
  assert.equal(isUnlinkedSession(bucketId), true);
  assert.equal(isUnlinkedSession('other-bucket', true), true);
  assert.equal(isUnlinkedSession('00000000-0000-4000-8000-000000000002'), false);
  assert.deepEqual(unlinkedRequestFilter(bucketId, keyId, source), source);
  assert.deepEqual(unlinkedRequestFilter(bucketId)?.conditions, [
    { field: 'key_id', operator: 'equals', value: { type: 'uuid', value: keyId } },
  ]);
  assert.equal(JSON.stringify(unlinkedRequestFilter(bucketId, keyId, source)).includes('unlinked:'), false);
});

test('bucket navigation refuses ambiguous identities and OR scopes', () => {
  assert.equal(unlinkedRequestFilter(bucketId, '00000000-0000-4000-8000-000000000001'), undefined);
  assert.equal(unlinkedRequestFilter('unlinked:invalid'), undefined);
  assert.equal(unlinkedRequestFilter('unlinked:'), undefined);
  assert.equal(unlinkedRequestFilter(bucketId, keyId, { logical_operator: 'or', conditions: [] }), undefined);
});
