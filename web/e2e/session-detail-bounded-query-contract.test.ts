import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

test('session detail limits both sources before archive and observation joins', async () => {
  const source = await readFile(new URL('../../src/db/requests/conversations.rs', import.meta.url), 'utf8');
  const queries = [...source.matchAll(/"SELECT requests\.\*[^\n]+/g)].map(match => match[0]);
  assert.equal(queries.length, 2);
  for (const query of queries) {
    const cursor = query.includes('LIMIT $5');
    const limit = cursor ? '$5' : '$3';
    assert.ok(query.includes(`ORDER BY created_at DESC, id DESC LIMIT ${limit}) r LEFT JOIN request_archive_spools`));
    assert.ok(query.includes(`ORDER BY source_started_at DESC, archive_request_id DESC LIMIT ${limit}) archive_page) requests LEFT JOIN conversation_observations`));
    if (cursor) {
      assert.ok(query.includes('AND (created_at < $3 OR (created_at = $3 AND id < $4))'));
      assert.ok(query.includes('AND (source_started_at < $3 OR (source_started_at = $3 AND archive_request_id < $4))'));
    }
  }
});
