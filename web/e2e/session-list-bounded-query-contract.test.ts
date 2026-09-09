import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

test('latest session metadata resolves IDs once per bounded page before fetching history rows', async () => {
  const sql = await readFile(new URL('../../src/db/session_analytics.rs', import.meta.url), 'utf8');
  const materialized = sql.indexOf('latest_ids AS MATERIALIZED');
  const recentActivity = sql.indexOf('recent_activity AS (');
  assert.ok(materialized > sql.indexOf('LIMIT $3'));
  assert.ok(recentActivity > materialized);
  const history = sql.slice(recentActivity, sql.indexOf('latest_activity AS ('));
  assert.equal((history.match(/FROM latest_ids recent/g) ?? []).length, 4);
  assert.doesNotMatch(history, /SELECT latest/);
  assert.match(sql.slice(materialized, recentActivity), /conversation_cluster_id IS NULL/);
});
